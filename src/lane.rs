use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use gtk::gdk;
use gtk::prelude::*;
use nostr_sdk::nostr::secp256k1::XOnlyPublicKey;
use nostr_sdk::nostr::{Event, Sha256Hash};
// use nostr_sdk::sqlite::model::Profile;
use relm4::factory::AsyncFactoryComponent;
use relm4::factory::FactoryVecDeque;
use relm4::prelude::*;
use relm4::{gtk, AsyncFactorySender};
use reqwest::Url;

use crate::nostr::{EventExt, Persona};
use crate::ui::details::Details;
use crate::ui::note::{Note, NoteInit, NoteInput};
use crate::win::Msg;

#[derive(Debug)]
pub struct Lane {
    central_note: Option<Sha256Hash>,
    text_notes: FactoryVecDeque<Note>,
    hash_index: HashMap<Sha256Hash, DynamicIndex>,
}

#[derive(Clone, Debug)]
pub enum LaneMsg {
    NewTextNote {
        event: Rc<Event>,
        // profile: Option<Profile>,
    },
    UpdatedProfile {
        author: Persona,
        metadata_json: Arc<String>,
    },
    ShowDetails(Details),
    AvatarBitmap {
        pubkey: XOnlyPublicKey,
        bitmap: Arc<gdk::Texture>,
    },
    Reaction {
        event: Sha256Hash,
        reaction: String,
    },
    Nip05Verified(XOnlyPublicKey),
    LinkClicked(Url),
}

#[derive(Debug)]
pub enum LaneOutput {
    ShowDetails(Details),
}

#[relm4::factory(pub async)]
impl AsyncFactoryComponent for Lane {
    type Init = Option<Sha256Hash>;
    type Input = LaneMsg;
    type Output = LaneOutput;
    type CommandOutput = ();
    type ParentInput = Msg;
    type ParentWidget = gtk::Box;

    view! {
        gtk::ScrolledWindow {
            set_hscrollbar_policy: gtk::PolicyType::Never,
            set_min_content_width: 600,
            set_hexpand: true,
            #[wrap(Some)]
            set_child = self.text_notes.widget() {}
        }
    }

    async fn init_model(
        central_note: Self::Init,
        _index: &DynamicIndex,
        sender: AsyncFactorySender<Self>,
    ) -> Self {
        Self {
            central_note,
            text_notes: FactoryVecDeque::new(
                gtk::ListBox::builder()
                    .selection_mode(gtk::SelectionMode::None)
                    .build(),
                sender.input_sender(),
            ),
            hash_index: Default::default(),
        }
    }

    fn output_to_parent_input(output: Self::Output) -> Option<Self::ParentInput> {
        match output {
            LaneOutput::ShowDetails(details) => Some(Msg::ShowDetail(details)),
        }
    }

    async fn update(&mut self, msg: Self::Input, sender: AsyncFactorySender<Self>) {
        match msg {
            LaneMsg::ShowDetails(details) => {
                sender.output(LaneOutput::ShowDetails(details));
            }

            LaneMsg::UpdatedProfile {
                author,
                metadata_json,
            } => self.text_notes.broadcast(NoteInput::UpdatedProfile {
                author,
                metadata_json,
            }),

            LaneMsg::AvatarBitmap { pubkey, bitmap } => self
                .text_notes
                .broadcast(NoteInput::AvatarBitmap { pubkey, bitmap }),

            LaneMsg::Reaction { event, reaction } => self
                .text_notes
                .broadcast(NoteInput::Reaction { event, reaction }),

            LaneMsg::Nip05Verified(pubkey) => {
                self.text_notes.broadcast(NoteInput::Nip05Verified(pubkey))
            }

            LaneMsg::NewTextNote { event } => self.text_note_received(event),
            LaneMsg::LinkClicked(uri) => println!("Clicked: {uri}"),
        }
    }
}

impl Lane {
    /// New text note was received, let's handle it.
    fn text_note_received(&mut self, event: Rc<Event>) {
        let event_id = event.id;

        // If `event` is a reply to a note, deliver it to the note to which
        // it replies.
        event
            .replies_to()
            .and_then(|hash| self.hash_index.get(&hash))
            .iter()
            .for_each(|&idx| {
                self.text_notes
                    .send(idx.current_index(), NoteInput::Reply(event.clone()))
            });

        // Add note iff it has not been added yet (they may arrive multiple times).
        if !self.hash_index.contains_key(&event.id) {
            let is_central = self.central_note == Some(event_id);
            let event_time = event.created_at;

            let init = NoteInit { event, is_central };

            let di = if is_central {
                // Central text note always goes first.
                self.text_notes.guard().push_front(init)
            } else {
                // Find index of first text note that was created later
                // than the text note being inserted.
                let idx = self
                    .text_notes
                    .iter()
                    .position(|tn| tn.time.timestamp() as u64 > event_time);

                if let Some(idx) = idx {
                    // Inserting somewhere in the middle.
                    self.text_notes.guard().insert(idx, init)
                } else {
                    // Appending to the end.
                    self.text_notes.guard().push_back(init)
                }
            };

            // At the end, let's remember (event_id -> dynamic index) pair.
            self.hash_index.insert(event_id, di);
        }
    }
}
