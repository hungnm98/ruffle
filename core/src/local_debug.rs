//! Opt-in host-page inspection/calls. Game-specific behavior belongs in host JS.
use crate::avm2::object::TObject;
use crate::avm2::property::Property;
use crate::avm2::{Activation, Avm2, EventObject, FunctionArgs, Value};
use crate::context::UpdateContext;
use crate::display_object::{DisplayObject, TDisplayObject, TDisplayObjectContainer};
use crate::events::MouseButton;
use crate::orphan_manager::OrphanManager;
use crate::external::Value as External;
use crate::string::AvmString;
use std::collections::BTreeMap;

fn field<'a>(object: &'a External, key: &str) -> Option<&'a External> {
    match object {
        External::Object(fields) => fields.get(key),
        _ => None,
    }
}
fn string(value: Option<&External>) -> Option<&str> {
    match value {
        Some(External::String(value)) => Some(value),
        _ => None,
    }
}

fn snapshot<'gc>(
    activation: &mut Activation<'_, 'gc>,
    value: Value<'gc>,
    depth: usize,
    budget: &mut usize,
) -> Result<External, String> {
    if *budget == 0 || depth > 8 {
        return Ok(External::String("<snapshot-limit>".into()));
    }
    *budget -= 1;
    if let Value::Object(object) = value {
        if let Some(array) = object.as_array_storage() {
            let values: Vec<_> = (0..array.length().min(10000))
                .map(|i| array.get(i).unwrap_or(Value::Undefined))
                .collect();
            drop(array);
            return values
                .into_iter()
                .map(|v| snapshot(activation, v, depth + 1, budget))
                .collect::<Result<Vec<_>, _>>()
                .map(External::List);
        }
        let mut fields = BTreeMap::new();
        let mut index = object
            .get_next_enumerant(0, activation)
            .map_err(|e| e.to_string(activation))?;
        while index != 0 && *budget > 0 {
            let name = object
                .get_enumerant_name(index, activation)
                .and_then(|v| v.coerce_to_string(activation))
                .map_err(|e| e.to_string(activation))?;
            let item = object
                .get_enumerant_value(index, activation)
                .map_err(|e| e.to_string(activation))?;
            fields.insert(
                name.to_string(),
                snapshot(activation, item, depth + 1, budget)?,
            );
            index = object
                .get_next_enumerant(index, activation)
                .map_err(|e| e.to_string(activation))?;
        }
        if index != 0 {
            fields.insert("__snapshotLimit".into(), External::Bool(true));
        }
        return Ok(External::Object(fields));
    }
    External::from_avm2(activation, value).map_err(|e| e.to_string(activation))
}

fn execute(context: &mut UpdateContext<'_>, request: External) -> Result<External, String> {
    let class_name = string(field(&request, "className")).ok_or("className is required")?;
    let Some(External::List(steps)) = field(&request, "steps") else {
        return Err("steps must be a list".into());
    };
    if steps.len() > 24 {
        return Err("Too many path steps".into());
    }
    let domains: Vec<_> = context
        .library
        .known_movies()
        .filter_map(|movie| {
            context
                .library
                .library_for_movie(movie)
                .and_then(|lib| lib.try_avm2_domain())
        })
        .collect();
    let mut selected = None;
    for domain in domains {
        let mut activation = Activation::from_domain(context, domain);
        let name = AvmString::new_utf8(activation.gc(), class_name);
        if domain.has_defined_value_handling_vector(&mut activation, name) {
            let value = domain
                .get_defined_value_handling_vector(&mut activation, name)
                .map_err(|e| e.to_string(&mut activation))?;
            selected = Some((domain, value));
            break;
        }
    }
    let (domain, mut value) = selected.ok_or("Class not loaded")?;
    let mut activation = Activation::from_domain(context, domain);
    for step in steps {
        if matches!(value, Value::Null | Value::Undefined) {
            return Err("Path reached null/undefined".into());
        }
        if let Some(name) = string(field(step, "get")) {
            let name = AvmString::new_utf8(activation.gc(), name);
            value = value
                .get_public_property(name, &mut activation)
                .map_err(|e| e.to_string(&mut activation))?;
        } else if let Some(name) = string(field(step, "call")) {
            let args = match field(step, "args") {
                Some(External::List(args)) => args.clone(),
                None => vec![],
                _ => return Err("args must be a list".into()),
            };
            let args: Vec<_> = args
                .into_iter()
                .map(|v| v.into_avm2(activation.context))
                .collect();
            let name = AvmString::new_utf8(activation.gc(), name);
            value = value
                .call_public_property(name, FunctionArgs::from_slice(&args), &mut activation)
                .map_err(|e| e.to_string(&mut activation))?;
        } else {
            return Err("A step needs get or call".into());
        }
    }
    // A real left click on the display object the path reached: the movie's own listeners
    // (with currentTarget set) decide what happens, as when a player clicks it.
    if matches!(field(&request, "click"), Some(External::Bool(true))) {
        let object = value.as_object().ok_or("click requires a display object")?;
        let target = object.as_display_object().ok_or("click requires a display object")?;
        let name = AvmString::new_utf8(activation.gc(), "click");
        let event = EventObject::mouse_event(&mut activation, name, target, None, 0, true, MouseButton::Left);
        return Ok(External::Bool(Avm2::dispatch_event(activation.context, event, object)));
    }
    let mut budget = 20000;
    // Explicit, read-only access to stored traits for the opt-in local debugger.
    // Reject ambiguous names rather than guessing a superclass/private namespace.
    if let Some(External::List(keys)) = field(&request, "slots") {
        let Value::Object(object) = value else {
            return Err("slots requires an object".into());
        };
        let mut output = BTreeMap::new();
        for key in keys {
            let External::String(key) = key else {
                return Err("slot names must be strings".into());
            };
            let mut matches = object.vtable().resolved_traits().iter().filter_map(
                |(name, _, property)| {
                    if name.to_string() != *key {
                        return None;
                    }
                    match property {
                        Property::Slot { slot_id } | Property::ConstSlot { slot_id } => Some(*slot_id),
                        _ => None,
                    }
                },
            );
            let slot_id = matches.next().ok_or_else(|| format!("Slot not found: {key}"))?;
            if matches.next().is_some() {
                return Err(format!("Ambiguous slot: {key}"));
            }
            output.insert(key.clone(), snapshot(&mut activation, object.get_slot(slot_id), 0, &mut budget)?);
        }
        return Ok(External::Object(output));
    }
    if let Some(External::List(keys)) = field(&request, "pick") {
        // Reading a property of null/undefined panics in Value::vtable and kills the player.
        if matches!(value, Value::Null | Value::Undefined) {
            return Err("Path reached null/undefined".into());
        }
        let mut output = BTreeMap::new();
        for key in keys {
            let External::String(key) = key else {
                return Err("pick keys must be strings".into());
            };
            let name = AvmString::new_utf8(activation.gc(), key);
            let item = value
                .get_public_property(name, &mut activation)
                .map_err(|e| e.to_string(&mut activation))?;
            output.insert(
                key.clone(),
                snapshot(&mut activation, item, 0, &mut budget)?,
            );
        }
        Ok(External::Object(output))
    } else if matches!(field(&request, "discard"), Some(External::Bool(true))) {
        Ok(External::Null)
    } else {
        snapshot(&mut activation, value, 0, &mut budget)
    }
}

fn descendants(dobj: DisplayObject<'_>, budget: &mut usize) -> usize {
    let mut count = 0;
    if let Some(container) = dobj.as_container() {
        for child in container.iter_render_list() {
            if *budget == 0 {
                break;
            }
            *budget -= 1;
            count += 1 + descendants(child, budget);
        }
    }
    count
}

// AVM2 orphans (clips with no parent whose frames Ruffle still runs every frame), grouped by class:
// { total, classes: { name: [count, descendants, first instance names] } }.
fn orphans(context: &mut UpdateContext<'_>) -> External {
    let mut classes: BTreeMap<String, (usize, usize, Vec<String>)> = BTreeMap::new();
    let mut total = 0;
    let mut budget = 200000;
    OrphanManager::each_orphan_obj(context, |dobj, context| {
        total += 1;
        let class = dobj
            .object2_or_null()
            .as_object()
            .map(|o| o.instance_of_class_name(context.gc()).to_string())
            .unwrap_or_else(|| "<no avm2 object>".into());
        let entry = classes.entry(class).or_default();
        entry.0 += 1;
        entry.1 += descendants(dobj, &mut budget);
        if entry.2.len() < 5 {
            entry.2.push(dobj.name().map(|n| n.to_string()).unwrap_or_default());
        }
    });
    let classes = classes
        .into_iter()
        .map(|(name, (count, below, names))| {
            let names = External::List(names.into_iter().map(External::String).collect());
            (name, External::List(vec![External::Number(count as f64), External::Number(below as f64), names]))
        })
        .collect();
    External::Object(BTreeMap::from([
        ("total".into(), External::Number(total as f64)),
        ("classes".into(), External::Object(classes)),
    ]))
}

pub fn run(context: &mut UpdateContext<'_>, request: External) -> External {
    if matches!(field(&request, "orphans"), Some(External::Bool(true))) {
        let value = orphans(context);
        return External::Object(BTreeMap::from([("ok".into(), External::Bool(true)), ("value".into(), value)]));
    }
    let result = match execute(context, request) {
        Ok(value) => [("ok".into(), External::Bool(true)), ("value".into(), value)],
        Err(error) => [
            ("ok".into(), External::Bool(false)),
            ("error".into(), External::String(error)),
        ],
    };
    External::Object(BTreeMap::from(result))
}
