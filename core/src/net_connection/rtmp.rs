//! AMF0 command transport. RTMP chunking/handshake/encryption live in the bridge.
use super::{ResponderCallback, ResponderHandle};
use crate::avm2::FunctionArgs;
use crate::avm2::deserialize_value;
use crate::avm2::object::{EventObject, NetConnectionObject};
use crate::avm2::{Activation, Avm2, Value};
use crate::context::UpdateContext;
use crate::string::AvmString;
use async_channel::{Receiver, Sender, TryRecvError};
use flash_lso::amf0::read::AMF0Decoder;
use flash_lso::packet::{Message, Packet};
use flash_lso::types::{AMFVersion, Element, ObjectId, Value as AmfValue};
use std::collections::HashMap;
use std::rc::Rc;

#[derive(Debug)]
pub struct Rtmp {
    pub url: String,
    pub connected: bool,
    outgoing: Sender<Vec<u8>>,
    incoming: Receiver<Vec<u8>>,
    responders: HashMap<u32, ResponderHandle>,
    next_id: u32,
    ended: bool,
}

pub enum RtmpEvent {
    Status(Rc<AmfValue>),
    Result(ResponderHandle, ResponderCallback, Rc<AmfValue>),
    Call(String, Vec<Rc<AmfValue>>),
}

fn property(name: &str, value: AmfValue) -> Element {
    Element::new(name, Rc::new(value))
}

fn status(code: &str) -> Rc<AmfValue> {
    Rc::new(AmfValue::Object(
        ObjectId::INVALID,
        vec![
            property("code", AmfValue::String(code.into())),
            property(
                "level",
                AmfValue::String(
                    if code.ends_with("Success") || code.ends_with("Closed") {
                        "status"
                    } else {
                        "error"
                    }
                    .into(),
                ),
            ),
        ],
        None,
    ))
}

pub fn status_code(value: &AmfValue) -> Option<&str> {
    let AmfValue::Object(_, fields, _) = value else {
        return None;
    };
    fields
        .iter()
        .find_map(|field| match (&*field.value, field.name.as_str()) {
            (AmfValue::String(code), "code") => Some(code.as_str()),
            _ => None,
        })
}

// Apply only connection lifecycle codes; RPC status notifications are unrelated.
pub fn connected_after_status(connected: bool, info: &AmfValue) -> bool {
    match status_code(info) {
        Some("NetConnection.Connect.Success") => true,
        Some(
            "NetConnection.Connect.Closed"
            | "NetConnection.Connect.Failed"
            | "NetConnection.Connect.Rejected",
        ) => false,
        _ => connected,
    }
}

// flash-lso exposes the AMF packet encoder, not the AMF0 value encoder.
// An empty target/response packet header is 14 bytes; the strict-array wrapper
// adds 5 bytes. The remainder is the sequence of command arguments, without
// an HTTP-remoting envelope. Covered by raw-wire tests below.
fn encode(values: Vec<Rc<AmfValue>>) -> Vec<u8> {
    let packet = Packet {
        version: AMFVersion::AMF0,
        headers: vec![],
        messages: vec![Message {
            target_uri: String::new(),
            response_uri: String::new(),
            contents: Rc::new(AmfValue::StrictArray(ObjectId::INVALID, values)),
        }],
    };
    flash_lso::packet::write::write_to_bytes(&packet, true).expect("AMF0 packet encoding")[19..]
        .to_vec()
}

fn command(name: &str, id: u32, object: Rc<AmfValue>, arguments: Vec<Rc<AmfValue>>) -> Vec<u8> {
    let mut values = vec![
        Rc::new(AmfValue::String(name.into())),
        Rc::new(AmfValue::Number(id as f64)),
        object,
    ];
    values.extend(arguments);
    encode(values)
}

fn decode(mut bytes: &[u8]) -> Result<Vec<Rc<AmfValue>>, ()> {
    let mut decoder = AMF0Decoder::default();
    let mut values = Vec::new();
    while !bytes.is_empty() {
        let (rest, value) = decoder.parse_single_element(bytes).map_err(|_| ())?;
        if rest.len() >= bytes.len() {
            return Err(());
        }
        values.push(value);
        bytes = rest;
    }
    Ok(values)
}

impl Rtmp {
    pub fn new(
        url: String,
        swf_url: String,
        arguments: Vec<Rc<AmfValue>>,
        outgoing: Sender<Vec<u8>>,
        incoming: Receiver<Vec<u8>>,
    ) -> Self {
        let app = url::Url::parse(&url)
            .map(|url| url.path().trim_matches('/').to_owned())
            .unwrap_or_default();
        let object = AmfValue::Object(
            ObjectId::INVALID,
            vec![
                property("app", AmfValue::String(app)),
                property("flashVer", AmfValue::String("WIN 32,0,0,465".into())),
                property("swfUrl", AmfValue::String(swf_url)),
                property("tcUrl", AmfValue::String(url.clone())),
                property("fpad", AmfValue::Bool(false)),
                property("capabilities", AmfValue::Number(239.0)),
                property("audioCodecs", AmfValue::Number(3575.0)),
                property("videoCodecs", AmfValue::Number(252.0)),
                property("videoFunction", AmfValue::Number(1.0)),
                property("objectEncoding", AmfValue::Number(0.0)),
            ],
            None,
        );
        let _ = outgoing.try_send(command("connect", 1, Rc::new(object), arguments));
        Self {
            url,
            connected: false,
            outgoing,
            incoming,
            responders: HashMap::new(),
            next_id: 2,
            ended: false,
        }
    }

    pub fn send(&mut self, name: String, responder: Option<ResponderHandle>, message: AmfValue) {
        let AmfValue::StrictArray(_, arguments) = message else {
            return;
        };
        let id = if let Some(responder) = responder {
            let id = self.next_id;
            self.next_id = self.next_id.checked_add(1).expect("RTMP transaction count");
            self.responders.insert(id, responder);
            id
        } else {
            0
        };
        if self
            .outgoing
            .try_send(command(&name, id, Rc::new(AmfValue::Null), arguments))
            .is_err()
        {
            self.outgoing.close();
        }
    }

    pub fn receive(&mut self) -> Vec<RtmpEvent> {
        let mut events = Vec::new();
        if self.ended {
            return events;
        }
        let mut received_connected = self.connected;
        // Limit per-frame work so server traffic cannot starve rendering.
        for _ in 0..256 {
            match self.incoming.try_recv() {
                Ok(bytes) => match decode(&bytes) {
                    Ok(values) if values.len() >= 3 => {
                        let AmfValue::String(name) = &*values[0] else {
                            continue;
                        };
                        let AmfValue::Number(transaction) = &*values[1] else {
                            continue;
                        };
                        if name == "_result" || name == "_error" {
                            let value = values
                                .get(3)
                                .cloned()
                                .unwrap_or_else(|| Rc::new(AmfValue::Null));
                            if *transaction == 1.0 {
                                received_connected =
                                    connected_after_status(received_connected, &value);
                                events.push(RtmpEvent::Status(value));
                            } else if let Some(responder) =
                                self.responders.remove(&(*transaction as u32))
                            {
                                events.push(RtmpEvent::Result(
                                    responder,
                                    if name == "_result" {
                                        ResponderCallback::Result
                                    } else {
                                        ResponderCallback::Status
                                    },
                                    value,
                                ));
                            }
                        } else if name == "onStatus" {
                            if let Some(info) = values.get(3) {
                                received_connected =
                                    connected_after_status(received_connected, info);
                                events.push(RtmpEvent::Status(info.clone()));
                            }
                        } else {
                            events.push(RtmpEvent::Call(name.clone(), values[3..].to_vec()));
                        }
                    }
                    _ => {
                        self.outgoing.close();
                        self.ended = true;
                        events.push(RtmpEvent::Status(status("NetConnection.Connect.Failed")));
                        break;
                    }
                },
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Closed) => {
                    self.ended = true;
                    self.responders.clear();
                    events.push(RtmpEvent::Status(status(if received_connected {
                        "NetConnection.Connect.Closed"
                    } else {
                        "NetConnection.Connect.Failed"
                    })));
                    break;
                }
            }
        }
        events
    }
}

pub fn dispatch<'gc>(
    context: &mut UpdateContext<'gc>,
    object: NetConnectionObject<'gc>,
    event: RtmpEvent,
) {
    if let RtmpEvent::Result(responder, callback, value) = event {
        responder.call(context, callback, value);
        return;
    }
    let mut activation = Activation::from_nothing(context);
    let result = (|| -> Result<(), crate::avm2::Error<'gc>> {
        match event {
            RtmpEvent::Status(info) => {
                let info = deserialize_value(&mut activation, &info)?;
                let class = activation.avm2().classes().netstatusevent;
                let name = AvmString::new_utf8(activation.gc(), "netStatus");
                let event = EventObject::from_class_and_args(
                    &mut activation,
                    class,
                    &[name.into(), false.into(), false.into(), info],
                );
                Avm2::dispatch_event(activation.context, event, object.into());
            }
            RtmpEvent::Call(name, arguments) => {
                let client = Value::from(object).get_public_property(
                    AvmString::new_utf8(activation.gc(), "client"),
                    &mut activation,
                )?;
                let arguments = arguments
                    .iter()
                    .map(|v| deserialize_value(&mut activation, v))
                    .collect::<Result<Vec<_>, _>>()?;
                let name = AvmString::new_utf8(activation.gc(), name);
                client.call_public_property(
                    name,
                    FunctionArgs::from_slice(&arguments),
                    &mut activation,
                )?;
            }
            RtmpEvent::Result(..) => unreachable!(),
        }
        Ok(())
    })();
    if let Err(error) = result {
        Avm2::uncaught_error(&mut activation, None, error, "RTMP callback");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn command_has_raw_amf0_wire_format() {
        let bytes = command(
            "test",
            0,
            Rc::new(AmfValue::Null),
            vec![Rc::new(AmfValue::Number(42.0))],
        );
        assert_eq!(&bytes[..7], &[2, 0, 4, b't', b'e', b's', b't']);
        let values = decode(&bytes).unwrap();
        assert_eq!(*values[3], AmfValue::Number(42.0));
        assert!(decode(&bytes[..bytes.len() - 1]).is_err());
    }
    #[test]
    fn connect_preserves_nested_arguments_and_requires_server_success() {
        let (tx, rx) = async_channel::bounded(10);
        let (server, incoming) = async_channel::bounded(10);
        let args = Rc::new(AmfValue::StrictArray(
            ObjectId::INVALID,
            vec![Rc::new(AmfValue::String("G".into()))],
        ));
        let mut connection = Rtmp::new(
            "rtmpe://localhost:80/master/test/".into(),
            "https://example.invalid/s/s48/GameLoaders.swf?version=test".into(),
            vec![args.clone()],
            tx,
            incoming,
        );
        assert!(!connection.connected);
        let values = decode(&rx.try_recv().unwrap()).unwrap();
        assert!(
            matches!(&*values[2], AmfValue::Object(_, fields, _) if fields.iter().any(|field| field.name == "app" && *field.value == AmfValue::String("master/test".into())))
        );
        // The server uses the originating SWF URL during connect. Omitting it
        // makes the game reject even otherwise valid arguments as SERVER_NOT_READY.
        assert!(
            matches!(&*values[2], AmfValue::Object(_, fields, _) if fields.iter().any(|field| field.name == "swfUrl" && *field.value == AmfValue::String("https://example.invalid/s/s48/GameLoaders.swf?version=test".into())))
        );
        assert!(matches!(&*values[3], AmfValue::StrictArray(_, items) if items.len() == 1));
        server
            .try_send(command(
                "_result",
                1,
                Rc::new(AmfValue::Null),
                vec![status("NetConnection.Connect.Success")],
            ))
            .unwrap();
        assert!(
            matches!(&connection.receive()[0], RtmpEvent::Status(info) if status_code(info) == Some("NetConnection.Connect.Success"))
        );
    }
    #[test]
    fn server_call_and_transport_failure_are_delivered() {
        let (tx, _rx) = async_channel::bounded(10);
        let (server, incoming) = async_channel::bounded(10);
        let mut connection = Rtmp::new(
            "rtmpe://localhost/app".into(),
            "file:///test.swf".into(),
            vec![],
            tx,
            incoming,
        );
        server
            .try_send(command("onCharacters", 0, Rc::new(AmfValue::Null), vec![]))
            .unwrap();
        assert!(
            matches!(&connection.receive()[0], RtmpEvent::Call(name, _) if name == "onCharacters")
        );
        drop(server);
        assert!(
            matches!(&connection.receive()[0], RtmpEvent::Status(info) if status_code(info) == Some("NetConnection.Connect.Failed"))
        );
        assert!(connection.receive().is_empty());
    }
    #[test]
    fn success_then_eof_in_one_frame_reports_closed_not_failed() {
        let (tx, _rx) = async_channel::bounded(10);
        let (server, incoming) = async_channel::bounded(10);
        let mut connection = Rtmp::new(
            "rtmpe://localhost/app".into(),
            "file:///test.swf".into(),
            vec![],
            tx,
            incoming,
        );
        server
            .try_send(command(
                "_result",
                1,
                Rc::new(AmfValue::Null),
                vec![status("NetConnection.Connect.Success")],
            ))
            .unwrap();
        drop(server);
        let events = connection.receive();
        assert_eq!(events.len(), 2);
        assert!(
            matches!(&events[1], RtmpEvent::Status(info) if status_code(info) == Some("NetConnection.Connect.Closed"))
        );
        assert!(connected_after_status(
            true,
            &status("NetConnection.Call.Failed")
        ));
        assert!(!connected_after_status(
            true,
            &status("NetConnection.Connect.Closed")
        ));
    }
}
