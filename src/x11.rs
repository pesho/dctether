//! Filter for the client → server half of a relayed X11 connection.
//!
//! The relay has to look at two things in that stream:
//!
//! 1. **The connection setup.** Container clients authenticate with a fake
//!    cookie the agent generated; the filter checks it and substitutes the
//!    host's real cookie, so the real one never enters the container
//!    (the same scheme `ssh -X` uses).
//! 2. **`QueryExtension` requests** for extensions that need to share memory
//!    or file descriptors with the server — `MIT-SHM` and `DRI3`. Neither
//!    can cross a byte relay, so the filter renames the queried extension
//!    to one that does not exist. The server then answers "not present"
//!    and clients take their non-shared-memory path. The request keeps its
//!    length, so sequence numbers are unaffected.
//!
//! Everything else passes through unchanged; the filter only tracks request
//! boundaries. Server → client data is not touched.

/// Extensions hidden from relayed clients.
pub const HIDDEN_EXTENSIONS: &[&[u8]] = &[b"MIT-SHM", b"DRI3"];

const OP_QUERY_EXTENSION: u8 = 98;
/// Upper bound on the setup packet (header + auth name + auth data).
const MAX_SETUP: usize = 12 + 2 * (u16::MAX as usize + 3);

#[derive(Debug, PartialEq, Eq)]
pub enum FilterError {
    BadByteOrder(u8),
    WrongCookie,
    BadRequestLength,
}

impl std::fmt::Display for FilterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FilterError::BadByteOrder(b) => write!(f, "X11 setup with invalid byte order {b:#x}"),
            FilterError::WrongCookie => write!(f, "X11 client did not present the session cookie"),
            FilterError::BadRequestLength => write!(f, "X11 request with invalid length"),
        }
    }
}

impl std::error::Error for FilterError {}

#[derive(Clone, Copy)]
enum Order {
    Big,
    Little,
}

impl Order {
    fn u16(self, b: &[u8]) -> u16 {
        match self {
            Order::Big => u16::from_be_bytes([b[0], b[1]]),
            Order::Little => u16::from_le_bytes([b[0], b[1]]),
        }
    }
    fn u32(self, b: &[u8]) -> u32 {
        match self {
            Order::Big => u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
            Order::Little => u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        }
    }
    fn put_u16(self, v: u16) -> [u8; 2] {
        match self {
            Order::Big => v.to_be_bytes(),
            Order::Little => v.to_le_bytes(),
        }
    }
}

fn pad4(n: usize) -> usize {
    (n + 3) & !3
}

pub struct ClientFilter {
    expected: (Vec<u8>, Vec<u8>),
    real: (Vec<u8>, Vec<u8>),
    order: Option<Order>,
    /// Bytes of the unit (setup or request) currently being examined.
    pending: Vec<u8>,
    /// Bytes of the current request still to pass through unexamined.
    passthrough: usize,
    failed: bool,
}

impl ClientFilter {
    /// `expected` is the (name, data) clients must present; `real` is what
    /// is sent to the server instead.
    pub fn new(expected: (Vec<u8>, Vec<u8>), real: (Vec<u8>, Vec<u8>)) -> Self {
        ClientFilter {
            expected,
            real,
            order: None,
            pending: vec![],
            passthrough: 0,
            failed: false,
        }
    }

    /// Move up to `n - pending.len()` bytes from `input` into `pending`.
    /// Returns true once `pending` holds `n` bytes.
    fn fill(&mut self, input: &mut &[u8], n: usize) -> bool {
        let want = n.saturating_sub(self.pending.len()).min(input.len());
        self.pending.extend_from_slice(&input[..want]);
        *input = &input[want..];
        self.pending.len() >= n
    }

    /// Process the next chunk of client bytes, appending what should go to
    /// the server to `out`. After an error the connection must be closed.
    pub fn feed(&mut self, mut input: &[u8], out: &mut Vec<u8>) -> Result<(), FilterError> {
        assert!(!self.failed, "feed after error");
        let r = self.feed_inner(&mut input, out);
        if r.is_err() {
            self.failed = true;
        }
        r
    }

    fn feed_inner(&mut self, input: &mut &[u8], out: &mut Vec<u8>) -> Result<(), FilterError> {
        loop {
            if self.passthrough > 0 {
                let n = self.passthrough.min(input.len());
                out.extend_from_slice(&input[..n]);
                *input = &input[n..];
                self.passthrough -= n;
            }
            if input.is_empty() {
                return Ok(());
            }
            match self.order {
                None => self.setup(input, out)?,
                Some(order) => self.request(order, input, out)?,
            }
        }
    }

    fn setup(&mut self, input: &mut &[u8], out: &mut Vec<u8>) -> Result<(), FilterError> {
        if !self.fill(input, 12) {
            return Ok(());
        }
        let order = match self.pending[0] {
            b'B' => Order::Big,
            b'l' => Order::Little,
            b => return Err(FilterError::BadByteOrder(b)),
        };
        let name_len = order.u16(&self.pending[6..8]) as usize;
        let data_len = order.u16(&self.pending[8..10]) as usize;
        let total = 12 + pad4(name_len) + pad4(data_len);
        debug_assert!(total <= MAX_SETUP);
        if !self.fill(input, total) {
            return Ok(());
        }
        let name = &self.pending[12..12 + name_len];
        let data_off = 12 + pad4(name_len);
        let data = &self.pending[data_off..data_off + data_len];
        if !ct_eq(name, &self.expected.0) || !ct_eq(data, &self.expected.1) {
            return Err(FilterError::WrongCookie);
        }
        let (rname, rdata) = &self.real;
        out.extend_from_slice(&self.pending[..6]);
        out.extend_from_slice(&order.put_u16(rname.len() as u16));
        out.extend_from_slice(&order.put_u16(rdata.len() as u16));
        out.extend_from_slice(&self.pending[10..12]);
        out.extend_from_slice(rname);
        out.resize(out.len() + pad4(rname.len()) - rname.len(), 0);
        out.extend_from_slice(rdata);
        out.resize(out.len() + pad4(rdata.len()) - rdata.len(), 0);
        self.pending.clear();
        self.order = Some(order);
        Ok(())
    }

    fn request(&mut self, order: Order, input: &mut &[u8], out: &mut Vec<u8>) -> Result<(), FilterError> {
        if !self.fill(input, 4) {
            return Ok(());
        }
        let short_len = order.u16(&self.pending[2..4]) as usize;
        let total = if short_len == 0 {
            // BIG-REQUESTS: the real length follows the header.
            if !self.fill(input, 8) {
                return Ok(());
            }
            let units = order.u32(&self.pending[4..8]) as usize;
            if units < 2 {
                return Err(FilterError::BadRequestLength);
            }
            units * 4
        } else {
            short_len * 4
        };

        if self.pending[0] == OP_QUERY_EXTENSION && short_len != 0 {
            if !self.fill(input, total) {
                return Ok(());
            }
            hide_extension(order, &mut self.pending);
        }
        out.extend_from_slice(&self.pending);
        self.passthrough = total - self.pending.len();
        self.pending.clear();
        Ok(())
    }
}

/// Rewrite a complete `QueryExtension` request in place if it asks for a
/// hidden extension. Layout: opcode, pad, length (2), name length (2),
/// pad (2), name.
fn hide_extension(order: Order, req: &mut [u8]) {
    if req.len() < 8 {
        return;
    }
    let n = order.u16(&req[4..6]) as usize;
    if 8 + n > req.len() {
        return;
    }
    let name = &mut req[8..8 + n];
    if HIDDEN_EXTENSIONS.iter().any(|h| *h == &name[..]) {
        // Same length, so the request size is unchanged. No extension name
        // starts with '!'.
        name[0] = b'!';
    }
}

pub(crate) fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAME: &[u8] = b"MIT-MAGIC-COOKIE-1";

    fn setup_packet(order: u8, name: &[u8], data: &[u8]) -> Vec<u8> {
        let put = |v: u16| if order == b'B' { v.to_be_bytes() } else { v.to_le_bytes() };
        let mut p = vec![order, 0];
        p.extend_from_slice(&put(11));
        p.extend_from_slice(&put(0));
        p.extend_from_slice(&put(name.len() as u16));
        p.extend_from_slice(&put(data.len() as u16));
        p.extend_from_slice(&[0, 0]);
        p.extend_from_slice(name);
        p.resize(12 + pad4(name.len()), 0);
        p.extend_from_slice(data);
        p.resize(12 + pad4(name.len()) + pad4(data.len()), 0);
        p
    }

    fn query_extension(order: u8, name: &[u8]) -> Vec<u8> {
        let put = |v: u16| if order == b'B' { v.to_be_bytes() } else { v.to_le_bytes() };
        let total = 8 + pad4(name.len());
        let mut r = vec![OP_QUERY_EXTENSION, 0];
        r.extend_from_slice(&put((total / 4) as u16));
        r.extend_from_slice(&put(name.len() as u16));
        r.extend_from_slice(&[0, 0]);
        r.extend_from_slice(name);
        r.resize(total, 0);
        r
    }

    fn filter() -> ClientFilter {
        ClientFilter::new((NAME.to_vec(), vec![1; 16]), (NAME.to_vec(), vec![2; 16]))
    }

    /// Feed `input` in chunks of `chunk` bytes.
    fn run(f: &mut ClientFilter, input: &[u8], chunk: usize) -> Result<Vec<u8>, FilterError> {
        let mut out = vec![];
        for c in input.chunks(chunk.max(1)) {
            f.feed(c, &mut out)?;
        }
        Ok(out)
    }

    #[test]
    fn swaps_cookie_in_setup_both_byte_orders() {
        for order in *b"lB" {
            for chunk in [1, 3, 7, 1000] {
                let mut f = filter();
                let out = run(&mut f, &setup_packet(order, NAME, &[1; 16]), chunk).unwrap();
                assert_eq!(out, setup_packet(order, NAME, &[2; 16]));
            }
        }
    }

    #[test]
    fn real_cookie_may_have_different_length() {
        let mut f = ClientFilter::new((NAME.to_vec(), vec![1; 16]), (vec![], vec![]));
        let out = run(&mut f, &setup_packet(b'l', NAME, &[1; 16]), 5).unwrap();
        assert_eq!(out, setup_packet(b'l', b"", b""));
    }

    #[test]
    fn rejects_wrong_or_missing_cookie() {
        let mut f = filter();
        assert_eq!(
            run(&mut f, &setup_packet(b'l', NAME, &[2; 16]), 100),
            Err(FilterError::WrongCookie)
        );
        let mut f = filter();
        assert_eq!(run(&mut f, &setup_packet(b'l', b"", b""), 100), Err(FilterError::WrongCookie));
        let mut f = filter();
        assert_eq!(
            run(&mut f, &setup_packet(b'l', NAME, &[1; 15]), 100),
            Err(FilterError::WrongCookie)
        );
    }

    #[test]
    fn rejects_bad_byte_order() {
        let mut p = setup_packet(b'l', NAME, &[1; 16]);
        p[0] = b'x';
        assert_eq!(run(&mut filter(), &p, 100), Err(FilterError::BadByteOrder(b'x')));
    }

    #[test]
    fn hides_shm_and_dri3_and_passes_others() {
        for order in *b"lB" {
            let mut input = setup_packet(order, NAME, &[1; 16]);
            let mut expected = setup_packet(order, NAME, &[2; 16]);
            for (name, hidden) in [
                (&b"MIT-SHM"[..], &b"!IT-SHM"[..]),
                (b"RENDER", b"RENDER"),
                (b"DRI3", b"!RI3"),
                (b"MIT-SHM-X", b"MIT-SHM-X"),
                (b"BIG-REQUESTS", b"BIG-REQUESTS"),
            ] {
                input.extend(query_extension(order, name));
                expected.extend(query_extension(order, hidden));
            }
            for chunk in [1, 2, 5, 13, 4096] {
                assert_eq!(run(&mut filter(), &input, chunk).unwrap(), expected, "chunk {chunk}");
            }
        }
    }

    #[test]
    fn passes_ordinary_and_big_requests_untouched() {
        let mut input = setup_packet(b'l', NAME, &[1; 16]);
        let mut expected = setup_packet(b'l', NAME, &[2; 16]);
        // NoOperation (127), length 1.
        let noop = vec![127, 0, 1, 0];
        // A BIG-REQUESTS PutImage-like request whose body happens to contain
        // a QueryExtension("MIT-SHM") byte pattern that must not be touched.
        let body = query_extension(b'l', b"MIT-SHM");
        let units = (8 + body.len()) / 4;
        let mut big = vec![72, 0, 0, 0];
        big.extend_from_slice(&(units as u32).to_le_bytes());
        big.extend_from_slice(&body);
        // A long ordinary request with the same pattern inside.
        let mut long = vec![55, 0];
        long.extend_from_slice(&(((4 + body.len()) / 4) as u16).to_le_bytes());
        long.extend_from_slice(&body);
        for r in [&noop, &big, &long, &noop] {
            input.extend_from_slice(r);
            expected.extend_from_slice(r);
        }
        input.extend(query_extension(b'l', b"MIT-SHM"));
        expected.extend(query_extension(b'l', b"!IT-SHM"));
        for chunk in [1, 3, 8, 64, 100000] {
            assert_eq!(run(&mut filter(), &input, chunk).unwrap(), expected, "chunk {chunk}");
        }
    }

    #[test]
    fn rejects_big_request_with_impossible_length() {
        let mut input = setup_packet(b'l', NAME, &[1; 16]);
        input.extend_from_slice(&[72, 0, 0, 0, 1, 0, 0, 0]);
        assert_eq!(run(&mut filter(), &input, 100), Err(FilterError::BadRequestLength));
    }
}
