//! Device audio I/O: the bridge between the platform's audio callbacks and the engine.
//!
//! The engine works in one format only: 48 kHz, mono, i16 PCM, frames of 20 ms.

pub mod format;
pub mod ring;

#[cfg(test)]
mod tests {}
