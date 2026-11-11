//! `cat <file>`: copies a file to stdout with chained read_at → write_all.

use std::io;

use iorca::{EventLoop, File, stdout};

const CHUNK: usize = 16 * 1024;

fn main() -> io::Result<()> {
    let path = std::env::args().nth(1).expect("usage: cat <file>");
    let mut el = EventLoop::new()?;
    File::open(&mut el, path, |el, res| match res {
        Ok(file) => copy(el, file, 0, Vec::with_capacity(CHUNK)),
        Err(e) => eprintln!("open error: {e}"),
    });
    el.run()
}

fn copy(el: &mut EventLoop, file: File, offset: u64, buf: Vec<u8>) {
    let next = file.clone();
    file.read_at(el, buf, offset, move |el, res, buf| match res {
        Ok(0) => {} // EOF: the last clone drops here and the file closes.
        Ok(n) => stdout().write_all(el, buf, move |el, res, buf| match res {
            // `write_all` hands the buffer back cleared, ready for reuse.
            Ok(()) => copy(el, next, offset + n as u64, buf),
            Err(e) => eprintln!("write error: {e}"),
        }),
        Err(e) => eprintln!("read error: {e}"),
    });
}
