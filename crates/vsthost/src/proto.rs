//! What Ferrocast and the helper say to each other over the helper's stdin and stdout: a byte
//! saying what, a little-endian u32 length, then that many bytes.

use std::io::{self, Read, Write};

/// Process a block: a u32 sequence number, then the left channel's f32s, then the right's
/// (little-endian). The answer is the same, processed.
pub const PROCESS: u8 = 1;
/// The plugin's state: a u32 length and the component's state, then the controller's.
pub const SET_STATE: u8 = 2;
/// Asks for the state; answered with `SET_STATE`'s layout under this op.
pub const GET_STATE: u8 = 3;
/// Opens the plugin's own window (answered with `OK` or `FAILED`).
pub const OPEN_WINDOW: u8 = 4;
/// Closes it.
pub const CLOSE_WINDOW: u8 = 5;
/// The helper says it's ready: a u32, the plugin's latency in samples, then its name (UTF-8).
pub const READY: u8 = 6;
/// Something didn't work: a message (UTF-8).
pub const FAILED: u8 = 7;
/// Done.
pub const OK: u8 = 8;
/// The helper says the plugin's state changed (its window): the client asks for it later.
pub const CHANGED: u8 = 9;

/// The most a frame may carry (a state, or 8192 stereo samples): a broken peer can't make the
/// other side allocate gigabytes.
pub const MAX_FRAME: usize = 64 << 20;

pub fn write_frame(w: &mut impl Write, op: u8, payload: &[u8]) -> io::Result<()> {
    let len = u32::try_from(payload.len()).map_err(|_| io::Error::other("frame too big"))?;
    let mut head = [0u8; 5];
    head[0] = op;
    head[1..].copy_from_slice(&len.to_le_bytes());
    w.write_all(&head)?;
    w.write_all(payload)?;
    w.flush()
}

pub fn read_frame(r: &mut impl Read) -> io::Result<(u8, Vec<u8>)> {
    let mut head = [0u8; 5];
    r.read_exact(&mut head)?;
    let len = u32::from_le_bytes([head[1], head[2], head[3], head[4]]) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::other(format!("a frame of {len} bytes")));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok((head[0], payload))
}

/// A block as sent: its number, then left, then right.
pub fn block_bytes(seq: u32, left: &[f32], right: &[f32]) -> Vec<u8> {
    let mut b = Vec::with_capacity(4 + (left.len() + right.len()) * 4);
    b.extend_from_slice(&seq.to_le_bytes());
    for s in left.iter().chain(right) {
        b.extend_from_slice(&s.to_le_bytes());
    }
    b
}

/// A block back: its number and its two channels (None when it's not one).
pub fn block_of(bytes: &[u8]) -> Option<(u32, Vec<f32>, Vec<f32>)> {
    let seq = u32::from_le_bytes(bytes.get(..4)?.try_into().ok()?);
    let samples = &bytes[4..];
    if !samples.len().is_multiple_of(8) {
        return None;
    }
    let all: Vec<f32> = samples.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect();
    let half = all.len() / 2;
    Some((seq, all[..half].to_vec(), all[half..].to_vec()))
}

/// A state: the component's and the controller's.
pub fn state_bytes(component: &[u8], controller: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(4 + component.len() + controller.len());
    b.extend_from_slice(&(component.len() as u32).to_le_bytes());
    b.extend_from_slice(component);
    b.extend_from_slice(controller);
    b
}

pub fn state_of(bytes: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let n = u32::from_le_bytes(bytes.get(..4)?.try_into().ok()?) as usize;
    let component = bytes.get(4..4 + n)?.to_vec();
    Some((component, bytes[4 + n..].to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_blocks_and_states_go_and_come_back() {
        let mut wire = Vec::new();
        write_frame(&mut wire, PROCESS, &block_bytes(7, &[0.5, -1.0], &[0.25, 2.0])).unwrap_or_else(|e| panic!("{e}"));
        let (op, payload) = read_frame(&mut wire.as_slice()).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(op, PROCESS);
        assert_eq!(block_of(&payload), Some((7, vec![0.5, -1.0], vec![0.25, 2.0])));
        assert_eq!(state_of(&state_bytes(b"abc", b"de")), Some((b"abc".to_vec(), b"de".to_vec())));
        assert_eq!(state_of(&[9, 0, 0, 0, 1]), None, "a length past the end");
        // A length too big is refused before anything is allocated.
        let mut huge = vec![PROCESS];
        huge.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(read_frame(&mut huge.as_slice()).is_err());
    }
}
