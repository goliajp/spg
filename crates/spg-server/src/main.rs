//! SPG daemon — v0.1: accept TCP connections, reply PONG to every PING.
//!
//! Single-writer transaction model and SQL execution come in later milestones;
//! this binary exists today just to prove the self-built wire protocol works
//! end-to-end against a real socket.

use std::env;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process;
use std::thread;

use spg_wire::{Frame, FrameError, Op, decode, encode};

const DEFAULT_ADDR: &str = "127.0.0.1:5544";
const READ_CHUNK: usize = 4096;

fn main() {
    let addr = env::args()
        .nth(1)
        .unwrap_or_else(|| DEFAULT_ADDR.to_string());
    if let Err(e) = run(&addr) {
        eprintln!("spg-server: fatal: {e}");
        process::exit(1);
    }
}

fn run(addr: &str) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    let local = listener.local_addr()?;
    eprintln!("spg-server: listening on {local}");

    for stream in listener.incoming() {
        let stream = stream?;
        thread::spawn(move || {
            let peer = stream.peer_addr().ok();
            if let Err(e) = handle(stream) {
                eprintln!("spg-server: conn {peer:?}: {e}");
            }
        });
    }
    Ok(())
}

fn handle(mut stream: TcpStream) -> std::io::Result<()> {
    let mut buf: Vec<u8> = Vec::with_capacity(READ_CHUNK);
    let mut chunk = [0u8; READ_CHUNK];

    loop {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Ok(()); // peer closed cleanly
        }
        buf.extend_from_slice(&chunk[..n]);

        // Drain as many complete frames as the buffer holds.
        loop {
            match decode(&buf) {
                Ok((frame, consumed)) => {
                    buf.drain(..consumed);
                    let reply = match frame.op {
                        Op::Ping => Frame::pong(),
                        Op::Pong | Op::Error => Frame::error("unexpected op from client in v0.1"),
                    };
                    write_frame(&mut stream, &reply)?;
                }
                Err(FrameError::ShortHeader | FrameError::ShortPayload) => break,
                Err(e) => {
                    let _ = write_frame(&mut stream, &Frame::error(&e.to_string()));
                    return Err(std::io::Error::other(e.to_string()));
                }
            }
        }
    }
}

fn write_frame(stream: &mut TcpStream, frame: &Frame) -> std::io::Result<()> {
    let mut out = Vec::with_capacity(16);
    encode(frame, &mut out).map_err(|e| std::io::Error::other(e.to_string()))?;
    stream.write_all(&out)
}
