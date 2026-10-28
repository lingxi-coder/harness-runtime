//! Assert the actual UniFFI metadata used for Swift and Kotlin code generation.

#![cfg(feature = "uniffi")]

use client::protocol::events::UNIFFI_META_CLIENT_ENUM_CLIENTEVENT;
use client::protocol::mod_session_event::UNIFFI_META_CLIENT_ENUM_MODSESSIONEVENTPAYLOAD;
use uniffi_meta::{Metadata, Type};

#[test]
fn mod_event_uniffi_exposes_every_payload_and_lossless_text_carrier() {
    let Metadata::Enum(payload) =
        uniffi_meta::read_metadata(&UNIFFI_META_CLIENT_ENUM_MODSESSIONEVENTPAYLOAD).unwrap()
    else {
        panic!("expected ModSessionEventPayload enum metadata");
    };
    assert_eq!(payload.name, "ModSessionEventPayload");
    assert_eq!(
        payload
            .variants
            .iter()
            .map(|v| v.name.as_str())
            .collect::<Vec<_>>(),
        ["Notice", "Log", "Toast", "Status", "Notify"]
    );
    let notify = payload.variants.last().unwrap();
    assert_eq!(
        notify
            .fields
            .iter()
            .map(|field| field.name.as_str())
            .collect::<Vec<_>>(),
        ["plugin", "text"]
    );
    assert!(matches!(
        &notify.fields[1].ty,
        Type::Record { name, .. } if name == "ClientTextDto"
    ));
    let status = &payload.variants[3];
    assert!(matches!(
        &status.fields[1].ty,
        Type::Optional { inner_type }
            if matches!(inner_type.as_ref(), Type::Record { name, .. } if name == "ClientTextDto")
    ));

    let Metadata::Enum(events) =
        uniffi_meta::read_metadata(&UNIFFI_META_CLIENT_ENUM_CLIENTEVENT).unwrap()
    else {
        panic!("expected ClientEvent enum metadata");
    };
    let event = events
        .variants
        .iter()
        .find(|variant| variant.name == "ModSessionEvent")
        .expect("ModSessionEvent metadata");
    assert_eq!(event.name, "ModSessionEvent");
    assert_eq!(
        event
            .fields
            .iter()
            .map(|field| field.name.as_str())
            .collect::<Vec<_>>(),
        ["session_id", "mod_turn_id", "payload"]
    );
    assert!(matches!(
        &event.fields[2].ty,
        Type::Enum { name, .. } if name == "ModSessionEventPayload"
    ));
}
