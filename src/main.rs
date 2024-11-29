use std::io::Read;

use shengji_types::ZSTD_ZSTD_DICT;
use tungstenite::{connect, Message};
use zstd::stream::read::Decoder;

fn main() {
    print!("Connecting to Shengji...");
    let (mut ws, _) = connect("wss://shengji.battery.aeturnalus.com/api").unwrap();
    println!(" done!");

    print!("Joining room...");
    let join_message = serde_json::to_string(&shengji::serving_types::JoinRoom {
        room_name: "80839240460fd944".to_string(),
        name: "autoshengji".to_string(),
    })
    .unwrap();
    ws.send(Message::Text(join_message)).unwrap();
    println!(" done!");

    let mut decompressor = zstd::bulk::Decompressor::with_dictionary(
        &zstd::bulk::decompress(ZSTD_ZSTD_DICT, 112_640).unwrap(),
    )
    .unwrap();

    loop {
        let result = ws.read().unwrap();
        println!("Received message: {:?}", result);
        match result {
            Message::Binary(data) => {
                let decompressed = decompressor
                    .decompress(&data, data.capacity() * 10)
                    .unwrap();
                println!(
                    "Decompressed: {:?}",
                    String::from_utf8(decompressed).unwrap()
                );
            }
            _ => {
                println!("Unexpected message type");
            }
        }
    }
}
