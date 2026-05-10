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
        Ok(u32::from_be_bytes(bytes.try_into().unwrap()))
    }

    pub(crate) fn u64(&mut self) -> Result<u64, XdrError> {
        let bytes = self.take(8)?;
        Ok(u64::from_be_bytes(bytes.try_into().unwrap()))
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
