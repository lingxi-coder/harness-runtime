//! One-way adapters from trusted media owners to native DTOs.
//!
//! The returned attachment ID fields describe the trusted owner for native
//! routing. They are not credentials and are deliberately never parsed back
//! into `MediaAttachmentId`; any future reverse path must authenticate against
//! the issuing host's attachment registry.

use client::protocol::audio::AudioOwnerDto;
use client::protocol::media::{MediaOwnerDto, MediaOwnerKindDto};
use media_api::{MediaAttachmentId, MediaOwner};

/// Convert a trusted host owner for the audio callback boundary.
pub(super) fn owner_to_audio_dto(owner: &MediaOwner) -> Result<AudioOwnerDto, &'static str> {
    let (attachment, kind, logical_owner_id) = owner_parts(owner)?;
    Ok(match attachment {
        Some(attachment_id) => AudioOwnerDto::Attached {
            attachment_host_id: attachment_id.host_id(),
            attachment_generation: attachment_id.generation(),
            logical_kind: kind,
            logical_owner_id: logical_owner_id.to_owned(),
        },
        None => match kind {
            MediaOwnerKindDto::Session => AudioOwnerDto::Session {
                session_id: logical_owner_id.to_owned(),
            },
            MediaOwnerKindDto::Ui => AudioOwnerDto::Ui {
                instance_id: logical_owner_id.to_owned(),
            },
            MediaOwnerKindDto::System => AudioOwnerDto::System {
                instance_id: logical_owner_id.to_owned(),
            },
        },
    })
}

/// Convert a trusted host owner for native media device and connection DTOs.
pub(super) fn owner_to_media_dto(owner: &MediaOwner) -> Result<MediaOwnerDto, &'static str> {
    let (attachment, kind, logical_owner_id) = owner_parts(owner)?;
    Ok(match attachment {
        Some(attachment_id) => MediaOwnerDto::Attached {
            attachment_host_id: attachment_id.host_id(),
            attachment_generation: attachment_id.generation(),
            logical_kind: kind,
            logical_owner_id: logical_owner_id.to_owned(),
        },
        None => match kind {
            MediaOwnerKindDto::Session => MediaOwnerDto::Session {
                session_id: logical_owner_id.to_owned(),
            },
            MediaOwnerKindDto::Ui => MediaOwnerDto::Ui {
                instance_id: logical_owner_id.to_owned(),
            },
            MediaOwnerKindDto::System => MediaOwnerDto::System {
                instance_id: logical_owner_id.to_owned(),
            },
        },
    })
}

fn owner_parts(
    owner: &MediaOwner,
) -> Result<(Option<&MediaAttachmentId>, MediaOwnerKindDto, &str), &'static str> {
    let (attachment, logical_owner) = match owner {
        MediaOwner::Attachment {
            attachment_id,
            owner,
        } => (Some(attachment_id), owner.as_ref()),
        owner => (None, owner),
    };

    let (kind, logical_owner_id) = match logical_owner {
        MediaOwner::Session { session_id } => (MediaOwnerKindDto::Session, session_id.as_str()),
        MediaOwner::Ui { instance_id } => (MediaOwnerKindDto::Ui, instance_id.as_str()),
        MediaOwner::System { instance_id } => (MediaOwnerKindDto::System, instance_id.as_str()),
        MediaOwner::Attachment { .. } => {
            return Err("nested media attachment owner cannot cross the native DTO boundary");
        }
    };
    Ok((attachment, kind, logical_owner_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attachment() -> MediaAttachmentId {
        media_api::MediaAttachmentIssuer::new()
            .issue()
            .expect("fresh issuer should issue an attachment")
    }

    #[test]
    fn attached_owner_conversions_preserve_scope_and_each_logical_owner_kind() {
        let attachment = attachment();
        let host_id = attachment.host_id();
        let generation = attachment.generation();
        let owners = [
            (
                MediaOwner::Session {
                    session_id: "session-1".into(),
                },
                MediaOwnerKindDto::Session,
                "session-1",
            ),
            (
                MediaOwner::Ui {
                    instance_id: "ui-1".into(),
                },
                MediaOwnerKindDto::Ui,
                "ui-1",
            ),
            (
                MediaOwner::System {
                    instance_id: "system-1".into(),
                },
                MediaOwnerKindDto::System,
                "system-1",
            ),
        ];

        for (owner, logical_kind, logical_owner_id) in owners {
            let owner = owner.in_attachment(attachment.clone());
            let audio = owner_to_audio_dto(&owner).unwrap();
            assert_eq!(
                audio,
                AudioOwnerDto::Attached {
                    attachment_host_id: host_id.clone(),
                    attachment_generation: generation,
                    logical_kind,
                    logical_owner_id: logical_owner_id.into(),
                }
            );
            assert!(audio.validate().is_ok());

            let media = owner_to_media_dto(&owner).unwrap();
            assert_eq!(
                media,
                MediaOwnerDto::Attached {
                    attachment_host_id: host_id.clone(),
                    attachment_generation: generation,
                    logical_kind,
                    logical_owner_id: logical_owner_id.into(),
                }
            );
            assert!(media.validate().is_ok());
        }
    }

    #[test]
    fn owner_conversions_preserve_unattached_owner_variants() {
        let owner = MediaOwner::Ui {
            instance_id: "ui-1".into(),
        };
        assert_eq!(
            owner_to_audio_dto(&owner).unwrap(),
            AudioOwnerDto::Ui {
                instance_id: "ui-1".into(),
            }
        );
        assert_eq!(
            owner_to_media_dto(&owner).unwrap(),
            MediaOwnerDto::Ui {
                instance_id: "ui-1".into(),
            }
        );
    }

    #[test]
    fn owner_conversions_reject_nested_attachment_without_losing_identity() {
        let owner = MediaOwner::System {
            instance_id: "system-1".into(),
        }
        .in_attachment(attachment())
        .in_attachment(attachment());
        assert!(owner_to_audio_dto(&owner).is_err());
        assert!(owner_to_media_dto(&owner).is_err());
    }
}
