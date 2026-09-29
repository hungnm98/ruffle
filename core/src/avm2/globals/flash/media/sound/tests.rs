use super::*;
use crate::avm2::QName;
use crate::avm2::object::SoundObjectHandle;
use crate::backend::navigator::{
    ErrorResponse, NavigationMethod, NavigatorBackend, NullNavigatorBackend, OwnedFuture,
    SuccessResponse,
};
use crate::loader::Error as LoaderError;
use crate::player::PlayerBuilder;
use crate::socket::{SocketAction, SocketHandle};
use crate::string::AvmString;
use async_channel::{Receiver, Sender};
use futures::FutureExt;
use indexmap::IndexMap;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;
use url::{ParseError, Url};

#[derive(Clone, Default)]
struct PendingNavigator {
    futures: Rc<RefCell<Vec<OwnedFuture<(), LoaderError>>>>,
    fetches: Rc<Cell<usize>>,
    dropped: Rc<Cell<usize>>,
}

struct DroppedFetch(Rc<Cell<usize>>);

impl Drop for DroppedFetch {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1);
    }
}

impl NavigatorBackend for PendingNavigator {
    fn navigate_to_url(
        &self,
        _: &str,
        _: &str,
        _: Option<(NavigationMethod, IndexMap<String, String>)>,
    ) {
    }

    fn fetch(&self, _: Request) -> OwnedFuture<Box<dyn SuccessResponse>, ErrorResponse> {
        self.fetches.set(self.fetches.get() + 1);
        let guard = DroppedFetch(self.dropped.clone());
        Box::pin(async move {
            let _guard = guard;
            std::future::pending().await
        })
    }

    fn resolve_url(&self, url: &str) -> Result<Url, ParseError> {
        Url::parse("https://example.invalid/").unwrap().join(url)
    }

    fn spawn_future(&mut self, future: OwnedFuture<(), LoaderError>) {
        self.futures.borrow_mut().push(future);
    }

    fn pre_process_url(&self, url: Url) -> Url {
        url
    }

    fn connect_socket(
        &mut self,
        host: String,
        port: u16,
        timeout: Duration,
        handle: SocketHandle,
        receiver: Receiver<Vec<u8>>,
        sender: Sender<SocketAction>,
    ) {
        NullNavigatorBackend::new().connect_socket(host, port, timeout, handle, receiver, sender);
    }
}

fn construct<'gc>(
    activation: &mut Activation<'_, 'gc>,
    name: &str,
    args: &[Value<'gc>],
) -> Value<'gc> {
    let name = AvmString::new_utf8(activation.gc(), name);
    let name = QName::from_qualified_name(name, activation.context);
    activation
        .avm2()
        .playerglobals_domain()
        .get_defined_value(activation, name)
        .unwrap()
        .as_object()
        .unwrap()
        .as_class_object()
        .unwrap()
        .construct(activation, args)
        .unwrap()
}

#[test]
fn close_cancels_before_fetch_and_during_fetch() {
    for start_fetch in [false, true] {
        let navigator = PendingNavigator::default();
        let player = PlayerBuilder::new()
            .with_navigator(navigator.clone())
            .build();
        let sound = player
            .lock()
            .unwrap()
            .mutate_with_update_context(|context| {
                let mut activation = Activation::from_nothing(context);
                // The game's empty-URL load/play/close sequence.
                let url = AvmString::new_utf8(activation.gc(), "");
                let request = construct(&mut activation, "flash.net.URLRequest", &[url.into()]);
                let sound = construct(&mut activation, "flash.media.Sound", &[request]);
                let result = play(
                    &mut activation,
                    sound,
                    FunctionArgs::from_slice(&[0.into(), 0.into(), Value::Null]),
                )
                .unwrap();
                assert!(!matches!(result, Value::Null));
                SoundObjectHandle::stash(
                    activation.context,
                    sound.as_object().unwrap().as_sound_object().unwrap(),
                )
            });
        let mut future = navigator.futures.borrow_mut().pop().unwrap();
        if start_fetch {
            assert!(future.as_mut().now_or_never().is_none());
            assert_eq!(navigator.fetches.get(), 1);
        }
        player
            .lock()
            .unwrap()
            .mutate_with_update_context(|context| {
                let sound = sound.fetch(context);
                let value = crate::avm2::Object::from(sound).into();
                let mut activation = Activation::from_nothing(context);
                close(&mut activation, value, FunctionArgs::empty()).unwrap();
                assert_eq!(sound.loading_state(), SoundLoadingState::Closed);
                assert!(sound.sound_handle().is_none());
                let error = close(&mut activation, value, FunctionArgs::empty()).unwrap_err();
                assert!(error.to_string(&mut activation).contains("#2029"));
            });
        // Cancellation completes successfully, drops the pending request, and cannot decode audio.
        future
            .now_or_never()
            .expect("cancelled load must not remain pending")
            .unwrap();
        assert_eq!(navigator.fetches.get(), usize::from(start_fetch));
        assert_eq!(navigator.dropped.get(), usize::from(start_fetch));
    }
}

#[test]
fn close_without_open_stream_preserves_loaded_sound() {
    let player = PlayerBuilder::new().build();
    player
        .lock()
        .unwrap()
        .mutate_with_update_context(|context| {
            let mut activation = Activation::from_nothing(context);
            let value = construct(&mut activation, "flash.media.Sound", &[]);
            let error = close(&mut activation, value, FunctionArgs::empty()).unwrap_err();
            assert!(error.to_string(&mut activation).contains("#2029"));
            let sound = value.as_object().unwrap().as_sound_object().unwrap();
            let handle = activation.context.audio.register_mp3(&[]).unwrap();
            sound.set_sound(activation.context, handle);
            let error = close(&mut activation, value, FunctionArgs::empty()).unwrap_err();
            assert!(error.to_string(&mut activation).contains("#2029"));
            assert_eq!(sound.sound_handle(), Some(handle));
            assert_eq!(sound.loading_state(), SoundLoadingState::Loaded);
        });
}
