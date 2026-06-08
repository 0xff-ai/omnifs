//! Minimal XDR helpers for the NFSv4.0 loopback subset.
//!
//! This module is deliberately small and local because the frontend only
//! decodes the handful of ONC RPC/NFS shapes implemented in `ops`. If protocol
//! coverage expands beyond that subset, revisit a generated or crate-backed XDR
//! layer instead of growing this reader into a general implementation.

pub(crate) fn usize_to_u32(value: usize) -> u32 {
    u32::try_from(value).expect("NFS XDR length exceeds u32")
}

#[derive(Debug)]
pub(crate) enum XdrError {
    Underflow,
    InvalidUtf8,
}

impl std::fmt::Display for XdrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Underflow => write!(f, "XDR underflow"),
            Self::InvalidUtf8 => write!(f, "invalid UTF-8 string"),
        }
    }
}

pub(crate) struct XdrReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> XdrReader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub(crate) fn u32(&mut self) -> Result<u32, XdrError> {
        let bytes = self.take(4)?;
        Ok(u32::from_be_bytes(
            bytes
                .try_into()
                .expect("XdrReader::take returned exact u32 length"),
        ))
    }

    pub(crate) fn u64(&mut self) -> Result<u64, XdrError> {
        let bytes = self.take(8)?;
        Ok(u64::from_be_bytes(
            bytes
                .try_into()
                .expect("XdrReader::take returned exact u64 length"),
        ))
    }

    pub(crate) fn string(&mut self) -> Result<String, XdrError> {
        let bytes = self.opaque()?;
        String::from_utf8(bytes).map_err(|_| XdrError::InvalidUtf8)
    }

    pub(crate) fn opaque(&mut self) -> Result<Vec<u8>, XdrError> {
        let len = self.u32()? as usize;
        let bytes = self.take(len)?.to_vec();
        self.skip_padding(len)?;
        Ok(bytes)
    }

    pub(crate) fn fixed_opaque(&mut self, len: usize) -> Result<Vec<u8>, XdrError> {
        let bytes = self.take(len)?.to_vec();
        self.skip_padding(len)?;
        Ok(bytes)
    }

    pub(crate) fn bitmap(&mut self) -> Result<Vec<u32>, XdrError> {
        let len = self.u32()? as usize;
        let mut words = Vec::with_capacity(len);
        for _ in 0..len {
            words.push(self.u32()?);
        }
        Ok(words)
    }

    pub(crate) fn fattr(&mut self) -> Result<(Vec<u32>, Vec<u8>), XdrError> {
        let bitmap = self.bitmap()?;
        let vals = self.opaque()?;
        Ok((bitmap, vals))
    }

    /// Skip XDR padding bytes after a variable-length opaque or string.
    ///
    /// Does **not** validate that the pad bytes are zero.  This is a deliberate
    /// choice for a subset server: strict pad validation would reject benign
    /// implementations that emit non-zero pad, and there is no security benefit
    /// from enforcing it in a loopback-only read-only frontend. If this server
    /// ever accepts network-facing connections, pad validation should be added.
    pub(crate) fn skip_padding(&mut self, len: usize) -> Result<(), XdrError> {
        let pad = (4 - (len % 4)) % 4;
        self.take(pad)?;
        Ok(())
    }

    pub(crate) fn take(&mut self, len: usize) -> Result<&'a [u8], XdrError> {
        if self.pos + len > self.data.len() {
            return Err(XdrError::Underflow);
        }
        let start = self.pos;
        self.pos += len;
        Ok(&self.data[start..start + len])
    }
}

pub(crate) struct XdrWriter {
    out: Vec<u8>,
}

impl XdrWriter {
    pub(crate) fn new() -> Self {
        Self { out: Vec::new() }
    }

    pub(crate) fn into_inner(self) -> Vec<u8> {
        self.out
    }

    pub(crate) fn len(&self) -> usize {
        self.out.len()
    }

    pub(crate) fn bytes(&mut self, bytes: &[u8]) {
        self.out.extend_from_slice(bytes);
    }

    pub(crate) fn u32(&mut self, value: u32) {
        self.out.extend_from_slice(&value.to_be_bytes());
    }

    pub(crate) fn u64(&mut self, value: u64) {
        self.out.extend_from_slice(&value.to_be_bytes());
    }

    pub(crate) fn i64(&mut self, value: i64) {
        self.out.extend_from_slice(&value.to_be_bytes());
    }

    pub(crate) fn bool(&mut self, value: bool) {
        self.u32(u32::from(value));
    }

    pub(crate) fn string(&mut self, value: &str) {
        self.opaque(value.as_bytes());
    }

    pub(crate) fn opaque(&mut self, bytes: &[u8]) {
        self.u32(usize_to_u32(bytes.len()));
        self.out.extend_from_slice(bytes);
        let pad = (4 - (bytes.len() % 4)) % 4;
        self.out.resize(self.out.len() + pad, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_all_truncations_fail(
        name: &str,
        bytes: &[u8],
        decode: impl Fn(&mut XdrReader<'_>) -> Result<(), XdrError>,
    ) {
        for len in 0..bytes.len() {
            let mut reader = XdrReader::new(&bytes[..len]);
            assert!(
                decode(&mut reader).is_err(),
                "{name} decoded truncated input of {len} bytes"
            );
        }
    }

    fn valid_attr_bytes() -> Vec<u8> {
        let mut writer = XdrWriter::new();
        writer.u32(2);
        writer.u32(0x0000_0001);
        writer.u32(0x8000_0000);
        writer.opaque(&[1, 2, 3]);
        writer.into_inner()
    }

    #[test]
    fn reader_rejects_truncated_supported_shapes() {
        let u32_bytes = 0x0102_0304_u32.to_be_bytes();
        assert_all_truncations_fail("u32", &u32_bytes, |reader| reader.u32().map(|_| ()));

        let u64_bytes = 0x0102_0304_0506_0708_u64.to_be_bytes();
        assert_all_truncations_fail("u64", &u64_bytes, |reader| reader.u64().map(|_| ()));

        let mut opaque = XdrWriter::new();
        opaque.opaque(b"abc");
        assert_all_truncations_fail("opaque", &opaque.into_inner(), |reader| {
            reader.opaque().map(|_| ())
        });

        let mut string = XdrWriter::new();
        string.string("hello");
        assert_all_truncations_fail("string", &string.into_inner(), |reader| {
            reader.string().map(|_| ())
        });

        let mut bitmap = XdrWriter::new();
        bitmap.u32(2);
        bitmap.u32(0x0000_0001);
        bitmap.u32(0x8000_0000);
        assert_all_truncations_fail("bitmap", &bitmap.into_inner(), |reader| {
            reader.bitmap().map(|_| ())
        });

        let fattr = valid_attr_bytes();
        assert_all_truncations_fail("fattr", &fattr, |reader| reader.fattr().map(|_| ()));
    }

    #[test]
    fn reader_rejects_invalid_utf8_strings() {
        let mut writer = XdrWriter::new();
        writer.opaque(&[0xff, 0xfe]);
        let bytes = writer.into_inner();
        let mut reader = XdrReader::new(&bytes);
        assert!(matches!(reader.string(), Err(XdrError::InvalidUtf8)));
    }
}
