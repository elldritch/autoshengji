use std::{iter::repeat, net::TcpStream};

use clap::Parser;
use rand::seq::SliceRandom as _;
use shengji::serving_types::{JoinRoom, UserMessage};
use shengji_core::{
    game_state::{self, GameState},
    interactive::Action,
};
use shengji_mechanics::{ordered_card::OrderedCard, trick::UnitLike, types::PlayerID};
use shengji_types::{GameMessage, ZSTD_ZSTD_DICT};
use tracing::{debug, info, instrument, trace};
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
    let bot_player_id = game_state.player_id(&argv.name).unwrap();

    loop {
        match game_state {
            // Do nothing, and wait for the game to start.
            GameState::Initialize(_) => trace!("Waiting for game to start"),

            // Draw cards on the bot's turn, but never bid.
            //
            // TODO: Implement bidding logic.
            GameState::Draw(draw_phase) => match draw_phase.next_player() {
                Ok(next_player_id) => {
                    if next_player_id == bot_player_id {
                        debug!("Drawing card");
                        socket.send(UserMessage::Action(Action::DrawCard));
                    } else {
                        debug!(?next_player_id, "Waiting for next player to draw");
                    }
                }
                Err(e) => {
                    if e.to_string() == "nobody has bid yet" {
                        debug!("Waiting for bids to be made")
                    } else {
                        panic!("Unexpected error during Draw phase: {}", e)
                    }
                }
            },

            // Do nothing. Since the bot never bids, it will never have to
            // exchange cards.
            GameState::Exchange(_) => trace!("Waiting for landlord to exchange cards"),

            // Play valid tricks at random.
            GameState::Play(play_phase) => {
                let trick = play_phase.trick();
                debug!(?trick, "Current trick");
                match trick.next_player() {
                    Some(next_player_id) => {
                        if next_player_id == bot_player_id {
                            debug!("Playing trick");
                            play_trick(&mut socket, &play_phase, bot_player_id);
                        } else {
                            debug!(?next_player_id, "Waiting for next player to play");
                        }
                    }
                    None => {
                        // This happens when the trick has ended (i.e. been won
                        // by somebody), but nobody has moved on to the next
                        // trick yet (i.e. hit the "Finish Trick" button, which
                        // sends the "EndTrick" action).
                        debug!("Waiting for next trick");
                    }
                }
            }
        }
        game_state = socket.read_state();
        trace!(?game_state, "New game state");
    }
}

#[instrument(level = "trace", skip(socket))]
fn play_trick(
    socket: &mut ShengjiSocket,
    play_phase: &game_state::play_phase::PlayPhase,
    bot_player_id: PlayerID,
) {
    let trick = play_phase.trick();
    let played_cards = trick.played_cards();
    let hands = play_phase.hands();
    let bot_hand = hands.get(bot_player_id).unwrap();
    debug!(?bot_hand, "Current hand");
    let bot_hand_cards = bot_hand
        .iter()
        .flat_map(|(card, count)| repeat(*card).take(*count))
        .collect::<Vec<_>>();

    match trick.trick_format() {
        None => {
            assert!(played_cards.len() == 0);
            debug!("Starting a new trick");

            // For now, just play a random card. Any one-card starting play will
            // always be valid.
            let card = bot_hand_cards.choose(&mut rand::thread_rng()).unwrap();
            debug!(?card, "Playing card");
            socket.send(UserMessage::Action(Action::PlayCards(vec![*card])));
        }
        Some(trick_format) => {
            assert!(played_cards.len() > 0);
            debug!(?trick_format, "Following trick format");

            let trick_draw_policy = play_phase.propagated().trick_draw_policy();
            let cards_in_trick_suit = bot_hand_cards
                .iter()
                .filter(|c| trick_format.trump().effective_suit(**c) == trick_format.suit())
                .copied()
                .collect::<Vec<_>>();

            let plays_within_suit = trick_format
                .decomposition(trick_draw_policy)
                .map(|format| {
                    UnitLike::check_play(
                        OrderedCard::make_map(
                            cards_in_trick_suit.iter().copied(),
                            trick_format.trump(),
                        ),
                        format.iter().cloned(),
                        trick_draw_policy,
                    )
                    .map(|units| {
                        units
                            .into_iter()
                            .flat_map(|u| {
                                u.into_iter()
                                    .flat_map(|(card, count)| repeat(card.card).take(count))
                                    .collect::<Vec<_>>()
                            })
                            .collect::<Vec<_>>()
                    })
                    .collect::<Vec<_>>()
                })
                .flatten()
                .collect::<Vec<_>>();

            debug!(?plays_within_suit, "Possible plays within the suit");

            // TODO: For truly random behavior, group the `plays_within_suit` by
            // Ord so that we know the set of hands with highest value (which
            // are the set we are required to pick from), and then pick randomly
            // among that set.
            let play = match plays_within_suit.first() {
                Some(p) => p,
                // If there are no plays available within the suit, then all
                // plays are valid.
                None => &bot_hand_cards
                    .choose_multiple(&mut rand::thread_rng(), trick_format.size())
                    .copied()
                    .collect(),
            };
            debug!(?play, "Playing cards");
            socket.send(UserMessage::Action(Action::PlayCards(play.clone())));
        }
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
