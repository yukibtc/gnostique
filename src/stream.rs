use std::path::PathBuf;
use std::time::Duration;

use futures_util::*;
use nostr_sdk::nostr::nips::nip05;
use nostr_sdk::prelude::{Event, EventId, Kind, SubscriptionFilter, XOnlyPublicKey};
use nostr_sdk::RelayPoolNotification;
use reqwest::Url;
use sqlx::query;
use tokio::sync::mpsc;
use tokio_stream::wrappers::{BroadcastStream, ReceiverStream};
use tracing::info;

use crate::nostr::{EventExt, Persona};
use crate::Gnostique;

#[derive(Debug)]
pub enum X {
    TextNote {
        event: Event,
        relays: Vec<Url>,
        author: Option<Persona>,
        avatar: Option<PathBuf>,
    },
    Reaction {
        event_id: EventId,
        content: String,
    },
    Metadata {
        persona: Persona,
        avatar: Option<PathBuf>,
    },
}

/// Requests requested by processing functions during processing incoming events.
#[derive(Debug)]
enum Feedback {
    /// Metadata for `pubkey` are requested from `relay`.
    NeedMetadata { relay: Url, pubkey: XOnlyPublicKey },
}

pub fn x(gnostique: &Gnostique) -> impl Stream<Item = X> + '_ {
    // A feedback from processing functions. If they need something,
    // they can ask by sending a message to `tx`.
    let (feedback, rx) = mpsc::channel(10);
    tokio::spawn(deal_with_feedback(gnostique.clone(), rx));

    BroadcastStream::new(gnostique.client().notifications())
        .filter_map(|r| async {
            if let Ok(RelayPoolNotification::Event(relay, event)) = r {
                Some((relay, event))
            } else {
                None
            }
        })
        .then(|(relay, event)| async {
            offer_relays(gnostique, &relay, &event).await;
            (relay, event)
        })
        .map(move |(relay, event)| received_event(gnostique, feedback.clone(), relay, event))
        .buffer_unordered(64)
        .filter_map(future::ready)
}

/// Listens to incoming messages asking for some additional actions or data
/// and processes them.
async fn deal_with_feedback(gnostique: Gnostique, rx: mpsc::Receiver<Feedback>) {
    ReceiverStream::new(rx)
        .for_each(|f| async {
            match f {
                Feedback::NeedMetadata { relay, pubkey } => {
                    // TODO: Batch requests?
                    let relays = gnostique.client().relays().await;
                    if let Some(r) = relays.get(&relay) {
                        r.req_events_of(
                            vec![SubscriptionFilter::new()
                                .kind(Kind::Metadata)
                                .author(pubkey)
                                .limit(1)],
                            Duration::from_secs(10),
                        );
                    }
                }
            }
        })
        .await
}

async fn received_event(
    gnostique: &Gnostique,
    feedback: mpsc::Sender<Feedback>,
    relay: Url,
    event: Event,
) -> Option<X> {
    match event.kind {
        Kind::TextNote => Some(received_text_note(gnostique, feedback, relay, event).await),
        Kind::Metadata => Some(received_metadata(gnostique, event).await),
        Kind::Reaction => event.reacts_to().map(|to| X::Reaction {
            event_id: to,
            content: event.content,
        }),
        _ => None,
    }
}

async fn received_metadata(gnostique: &Gnostique, event: Event) -> X {
    let pubkey_vec = event.pubkey.serialize().to_vec();
    let json = event.as_json().unwrap();

    let _ = query!(
        r#"
INSERT INTO metadata (author, event) VALUES (?, ?)
ON CONFLICT (author) DO UPDATE SET event = EXCLUDED.event
"#,
        pubkey_vec,
        json
    )
    .execute(gnostique.pool())
    .await;

    let metadata = event.as_metadata().unwrap();

    let avatar_url = metadata.picture.as_ref().and_then(|p| Url::parse(p).ok());
    let banner_url = metadata.banner.as_ref().and_then(|p| Url::parse(p).ok());

    // If the metadata's picture contains valid URL, download it.
    let avatar = if let Some(ref url) = avatar_url {
        Some(gnostique.download().cached_file(url).await)
    } else {
        None
    };

    let verified: bool = if let Some(ref nip05) = metadata.nip05 {
        verify_nip05(gnostique, event.pubkey, nip05).await
    } else {
        false
    };

    let p = Persona {
        pubkey: event.pubkey,
        name: metadata.name,
        avatar: avatar_url,
        banner: banner_url,
        about: metadata.about,
        nip05: metadata.nip05,
        nip05_verified: verified,
        metadata_json: json,
    };

    X::Metadata {
        persona: p,
        avatar: avatar.and_then(|d| d.file()),
    }
}

async fn received_text_note(
    gnostique: &Gnostique,
    feedback: mpsc::Sender<Feedback>,
    relay: Url,
    event: Event,
) -> X {
    gnostique.store_event(&relay, &event).await;
    let author = gnostique.get_persona(event.pubkey).await;

    let avatar = match &author {
        Some(Persona {
            avatar: Some(url), ..
        }) => {
            // Author is known and has a cached avatar
            gnostique.download().cached(url).await
        }
        Some(_) => {
            // We know the author but he has no avatar
            None
        }
        None => {
            // If we do not know the author yet, let us request his metadata.
            feedback
                .send(Feedback::NeedMetadata {
                    relay: relay.clone(),
                    pubkey: event.pubkey,
                })
                .await
                .unwrap_or_default();
            None
        }
    };

    let relays = gnostique.textnote_relays(event.id).await;

    X::TextNote {
        event,
        relays,
        author,
        avatar,
    }
}

async fn offer_relays(gnostique: &Gnostique, relay: &Url, event: &Event) {
    offer_relay_url(gnostique, relay).await;

    for r in event.collect_relays() {
        offer_relay_url(gnostique, &r).await
    }
}

async fn offer_relay_url(gnostique: &Gnostique, relay: &Url) {
    let url_s = relay.to_string();
    let _ = query!(
        "INSERT INTO relays(url) VALUES (?) ON CONFLICT(url) DO NOTHING",
        url_s
    )
    .execute(gnostique.pool())
    .await;
}

async fn verify_nip05(gnostique: &Gnostique, pubkey: XOnlyPublicKey, nip05: &str) -> bool {
    let pubkey_bytes = pubkey.serialize().to_vec();
    // If the nip05 is already verified and not for too long, just confirm.
    let x = query!(
        r#"
SELECT (unixepoch('now') - unixepoch(nip05_verified)) / 60 / 60 AS "hours?: u32"
FROM metadata WHERE author = ?"#,
        pubkey_bytes
    )
    .fetch_optional(gnostique.pool())
    .await;

    if let Ok(result) = x {
        let x = result.and_then(|r| r.hours);

        match x {
            Some(hours) if hours < 12 => {
                info!("NIP05: {} verified {} hours ago", nip05, hours);
                true
            }
            _ => {
                info!("NIP05: Verifying {}.", nip05);
                // If it's not yet verified or been verified for very long, update.
                if nip05::verify(pubkey, nip05, None).await.is_ok() {
                    let _ = query!(
                        r#"
UPDATE metadata SET nip05_verified = datetime('now')
WHERE author = ?"#,
                        pubkey_bytes
                    )
                    .execute(gnostique.pool())
                    .await;

                    info!("NIP05: {} verified.", nip05);
                    true
                } else {
                    info!("NIP05: {} verification failed.", nip05);
                    false
                }
            }
        }
    } else {
        false
    }
}
