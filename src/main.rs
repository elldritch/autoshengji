use std::net::TcpStream;

use clap::Parser;
use shengji::serving_types::{JoinRoom, UserMessage};
use shengji_core::{game_state::GameState, interactive::Action};
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
        debug!(?me, "Bot player");

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

    // TODO: Play phase
    // TODO: Start with random play?
    let span = span!(Level::INFO, "play_phase");
    let _enter = span.enter();
    info!(?game_state, "Entering Play phase");
    socket.chat("This is as far as I'm programmed so far. Goodbye!");
    todo!();
    loop {
        game_state = socket.read_state();
        trace!(?game_state, "Updated state");
        if let GameState::Play(p) = game_state {
            // if p.next_player().unwrap() == me.id {
            //     socket.send(UserMessage::Action(Action::PlayCard(0)));
            // }
        } else {
            panic!("Unexpected state during Play phase: {:#?}", game_state);
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
                decoded
            }
            _ => {
                panic!("Unexpected non-binary message from server: {:#?}", message);
            }
        }
    }
}
