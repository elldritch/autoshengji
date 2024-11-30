use std::{iter::repeat, net::TcpStream};

use clap::Parser;
use rand::seq::SliceRandom as _;
use shengji::serving_types::{JoinRoom, UserMessage};
use shengji_core::{game_state::GameState, interactive::Action};
use shengji_mechanics::{ordered_card::OrderedCard, trick::UnitLike, types::Card};
use shengji_types::{GameMessage, ZSTD_ZSTD_DICT};
use tracing::{debug, info, instrument, span, trace, Level};
use tracing_subscriber::{
    fmt::format::FmtSpan, layer::SubscriberExt as _, util::SubscriberInitExt as _,
};
use tungstenite::{stream::MaybeTlsStream, Message, WebSocket};

#[derive(Debug, Parser)]
struct Argv {
    /// Name of room to join
    #[arg(long)]
    room_name: String,

    /// Name of player
    #[arg(long, default_value_t = String::from("autoshengji"))]
    name: String,
}

#[instrument]
fn main() {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
                .with_file(true)
                .with_line_number(true)
                .with_target(true)
                .with_thread_ids(true)
                .with_thread_names(true)
                .with_writer(std::io::stderr)
                .pretty(),
        )
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let argv = Argv::parse();
    info!(?argv, "Parsed command-line arguments");

    let mut socket = ShengjiSocket::connect(argv.room_name, argv.name.clone());
    let mut game_state = socket.read_state();
    info!(?game_state, "Initial state");
    socket.chat(
        "Beep boop, I'm a bot! I just joined, so please give me a moment to get my bearings.",
    );

    // Start in the Initialize phase.
    //
    // TODO: I should just handle these phases statelessly, since I get the
    // whole game state for every phase anyway.
    let span = span!(Level::INFO, "initialize_phase");
    let me = {
        let _enter = span.enter();
        let me = if let GameState::Initialize(i) = game_state {
            socket.send(UserMessage::Ready);
            i.players()
                .iter()
                .find(|p| p.name == argv.name)
                .unwrap()
                .clone()
        } else {
            socket.chat(
                "It looks like the game has already started. I don't know how to handle this situation yet. Disconnecting!"
            );
            panic!("Expected Initialize phase, but got: {:#?}", game_state);
        };
        info!(?me, "Bot player");

        // Wait until we move onto the Draw phase. This might take a few state
        // updates, since each settings change in the Initialize phase results in a
        // state update.
        loop {
            game_state = socket.read_state();
            if let GameState::Draw(_) = game_state {
                break;
            } else if let GameState::Initialize(_) = game_state {
                debug!(?game_state, "Waiting for Draw phase");
            } else {
                panic!(
                    "Unexpected state during Initialize phase: {:#?}",
                    game_state
                );
            }
        }
        me
    };

    // In the Draw phase, draw cards when it's the bot's turn. Otherwise, wait
    // until the Exchange phase. This bot currently never bids.
    let span = span!(Level::INFO, "draw_phase");
    {
        let _enter = span.enter();
        info!(?game_state, "Entering Draw phase");
        loop {
            if let GameState::Draw(p) = game_state {
                match p.next_player() {
                    Ok(p) => {
                        trace!(?p, "Next player to draw");
                        if p == me.id {
                            debug!("Drawing card");
                            socket.send(UserMessage::Action(Action::DrawCard));
                        } else {
                            debug!(next_player_id = ?p, "Waiting for next player to draw");
                        }
                    }
                    Err(e) => {
                        if e.to_string() == "nobody has bid yet" {
                            debug!("Waiting for bids to be made")
                        } else {
                            panic!("Unexpected error during Draw phase: {}", e)
                        }
                    }
                }
            } else if let GameState::Exchange(_) = game_state {
                break;
            } else {
                panic!("Unexpected state during Draw phase: {:#?}", game_state);
            }
            game_state = socket.read_state();
            trace!(?game_state, "Updated state");
        }
    }

    // In the Exchange phase, do nothing, since the bot will never have the bid.
    let span = span!(Level::INFO, "exchange_phase");
    {
        let _enter = span.enter();
        info!(?game_state, "Entering Exchange phase");
        loop {
            game_state = socket.read_state();
            if let GameState::Play(_) = game_state {
                break;
            } else if let GameState::Exchange(_) = game_state {
                debug!(?game_state, "Waiting for Play phase");
            } else {
                panic!("Unexpected state during Exchange phase: {:#?}", game_state);
            }
        }
    }

    // In the Play phase, the bot plays random valid tricks.
    let span = span!(Level::INFO, "play_phase");
    let _enter = span.enter();
    info!(?game_state, "Entering Play phase");
    loop {
        if let GameState::Play(ref p) = game_state {
            let trick = p.trick();
            debug!(?trick, "Current trick");

            let played_cards = trick.played_cards();
            debug!(?played_cards, "Currently played cards");

            match trick.next_player() {
                Some(next_player_id) => {
                    if next_player_id != me.id {
                        debug!(?next_player_id, "Waiting for next player to play");
                    } else {
                        debug!("Playing trick");
                        let settings = p.propagated();
                        let hands = p.hands();
                        let current_hand_counts = hands.get(me.id).unwrap();
                        debug!(?current_hand_counts, "Current hand");
                        let current_hand = current_hand_counts
                            .iter()
                            .flat_map(|(card, count)| repeat(*card).take(*count))
                            .collect::<Vec<_>>();

                        match trick.trick_format() {
                            None => {
                                assert!(played_cards.len() == 0);
                                debug!("Starting a new trick");

                                // For now, just play a random card. Any one-card
                                // starting play will always be valid.
                                let card = current_hand.choose(&mut rand::thread_rng()).unwrap();
                                debug!(?card, "Playing card");
                                socket.send(UserMessage::Action(Action::PlayCards(vec![*card])));
                            }
                            Some(trick_format) => {
                                assert!(played_cards.len() > 0);
                                debug!(?trick_format, "Following this trick format");

                                let cards_in_trick_suit = current_hand
                                    .iter()
                                    .filter(|c| {
                                        trick_format.trump().effective_suit(**c)
                                            == trick_format.suit()
                                    })
                                    .copied()
                                    .collect::<Vec<_>>();

                                let matching_play = trick_format
                                    .decomposition(settings.trick_draw_policy())
                                    .filter_map(|format| {
                                        let mut playable = UnitLike::check_play(
                                            OrderedCard::make_map(
                                                current_hand.iter().copied(),
                                                trick_format.trump(),
                                            ),
                                            format.iter().cloned(),
                                            settings.trick_draw_policy(),
                                        );

                                        playable.next().map(|units| {
                                            units
                                                .iter()
                                                .flat_map(|unit| {
                                                    unit.iter().flat_map(|(card, count)| {
                                                        repeat(card.card).take(*count)
                                                    })
                                                })
                                                .collect::<Vec<_>>()
                                        })
                                    })
                                    .next();

                                // TODO: I took this from `core/examples/simulate_play.rs`.
                                // How the hell does this work?
                                //
                                // TODO: FIXME: I don't think this _does_ work. I think I need to write my own logic for this.
                                let num_required = trick_format.size();
                                let mut play = match matching_play {
                                    Some(matching) if matching.len() == num_required => matching,
                                    Some(_) if num_required >= cards_in_trick_suit.len() => {
                                        cards_in_trick_suit
                                    }
                                    Some(mut matching) => {
                                        // There are more available cards than required; we must at least
                                        let mut available_cards = cards_in_trick_suit;
                                        // pick the matching. Do this inefficiently!
                                        for m in &matching {
                                            available_cards.remove(
                                                available_cards
                                                    .iter()
                                                    .position(|c| *c == *m)
                                                    .unwrap(),
                                            );
                                        }
                                        available_cards.shuffle(&mut rand::thread_rng());
                                        matching.extend(
                                            available_cards[0..num_required - matching.len()]
                                                .iter()
                                                .copied(),
                                        );

                                        matching
                                    }
                                    None => cards_in_trick_suit,
                                };
                                let required_other_cards = num_required - play.len();
                                if required_other_cards > 0 {
                                    let mut other_cards =
                                        Card::cards(current_hand_counts.iter().filter(|(c, _)| {
                                            trick_format.trump().effective_suit(**c)
                                                != trick_format.suit()
                                        }))
                                        .copied()
                                        .collect::<Vec<_>>();
                                    other_cards.shuffle(&mut rand::thread_rng());
                                    play.extend(
                                        other_cards[0..required_other_cards].iter().copied(),
                                    );
                                }

                                debug!(?play, "Playing cards");
                                socket.send(UserMessage::Action(Action::PlayCards(play)));
                            }
                        }
                    }
                }
                None => {
                    // This happens when the trick has ended (i.e. been won by
                    // somebody), but nobody has moved on to the next trick yet
                    // (i.e. hit the "Finish Trick" button, which sends the
                    // "EndTrick" action).
                    debug!("Waiting for next trick");
                }
            }
        } else {
            panic!("Unexpected state during Play phase: {:#?}", game_state);
        }
        game_state = socket.read_state();
        trace!(?game_state, "Updated state");
    }
}

struct ShengjiSocket<'a> {
    ws: WebSocket<MaybeTlsStream<TcpStream>>,
    decompressor: zstd::bulk::Decompressor<'a>,
}

impl std::fmt::Debug for ShengjiSocket<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShengjiSocket")
            .field("ws", &self.ws)
            .finish_non_exhaustive()
    }
}

impl ShengjiSocket<'_> {
    #[instrument(level = "debug")]
    fn connect(room_name: String, name: String) -> Self {
        let decompressor = zstd::bulk::Decompressor::with_dictionary(
            &zstd::bulk::decompress(ZSTD_ZSTD_DICT, 112_640).unwrap(),
        )
        .unwrap();
        let (ws, _) = tungstenite::connect("wss://shengji.battery.aeturnalus.com/api").unwrap();

        let mut socket = ShengjiSocket { ws, decompressor };

        let join_message = serde_json::to_string(&JoinRoom { room_name, name }).unwrap();
        socket.ws.send(Message::Text(join_message)).unwrap();

        socket
    }

    #[instrument(level = "trace", skip(self))]
    fn send(&mut self, msg: UserMessage) {
        let message = serde_json::to_string(&msg).unwrap();
        trace!(?message, "Sending message");
        self.ws.send(Message::Text(message)).unwrap();
    }

    #[instrument(level = "trace", skip(self))]
    fn chat(&mut self, msg: &str) {
        self.send(UserMessage::Message(msg.to_owned()));
    }

    #[instrument(level = "trace", skip(self))]
    fn read_state(&mut self) -> GameState {
        loop {
            let msg = self.read_message();
            match msg {
                GameMessage::State { state } => {
                    return state;
                }
                _ => {}
            }
        }
    }

    #[instrument(level = "trace", skip(self))]
    fn read_message(&mut self) -> GameMessage {
        let message = self.ws.read().unwrap();
        match message {
            Message::Binary(data) => {
                trace!(?data, "Received binary message from server");
                let decompressed = self
                    .decompressor
                    .decompress(&data, data.capacity() * 10)
                    .unwrap();
                trace!(?decompressed, "Decompressed message");
                let decoded: GameMessage = serde_json::from_slice(&decompressed).unwrap();
                trace!(?decoded, "Decoded message");
                match decoded {
                    GameMessage::Error(err) => {
                        self.chat(&format!("Whoops, something went wrong! The error message is {:?}. Disconnecting.", err));
                        panic!("Error from server: {:#?}", err);
                    }
                    _ => {}
                }
                decoded
            }
            _ => {
                panic!("Unexpected message from server: {:#?}", message);
            }
        }
    }
}
