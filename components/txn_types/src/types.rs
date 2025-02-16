use super::timestamp::TimeStamp;
use byteorder::{ByteOrder, NativeEndian};
use kvproto::kvrpcpb;
use kvproto::pdpb;
use std::cell::RefCell;
use std::fmt::{self, Debug, Display, Formatter};
use std::sync::{Arc, RwLock};
use tikv_util::codec;
use tikv_util::codec::bytes;
use tikv_util::codec::bytes::BytesEncoder;
use tikv_util::codec::number::{self, NumberEncoder};
use tikv_util::collections::HashMap;

// Short value max len must <= 255.
pub const SHORT_VALUE_MAX_LEN: usize = 255;
pub const SHORT_VALUE_PREFIX: u8 = b'v';

pub fn is_short_value(value: &[u8]) -> bool {
    value.len() <= SHORT_VALUE_MAX_LEN
}

/// Value type which is essentially raw bytes.
pub type Value = Vec<u8>;

/// Key-value pair type.
///
/// The value is simply raw bytes; the key is a little bit tricky, which is
/// encoded bytes.
pub type KvPair = (Vec<u8>, Value);

/// Key type.
///
/// Keys have 2 types of binary representation - raw and encoded. The raw
/// representation is for public interface, the encoded representation is for
/// internal storage. We can get both representations from an instance of this
/// type.
///
/// Orthogonal to binary representation, keys may or may not embed a timestamp,
/// but this information is transparent to this type, the caller must use it
/// consistently.
#[derive(Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct Key(Vec<u8>);

/// Core functions for `Key`.
impl Key {
    /// Creates a key from raw bytes.
    #[inline]
    pub fn from_raw(key: &[u8]) -> Key {
        // adding extra length for appending timestamp
        let len = codec::bytes::max_encoded_bytes_size(key.len()) + codec::number::U64_SIZE;
        let mut encoded = Vec::with_capacity(len);
        encoded.encode_bytes(key, false).unwrap();
        Key(encoded)
    }

    /// Gets and moves the raw representation of this key.
    #[inline]
    pub fn into_raw(self) -> Result<Vec<u8>, codec::Error> {
        let mut k = self.0;
        bytes::decode_bytes_in_place(&mut k, false)?;
        Ok(k)
    }

    /// Gets the raw representation of this key.
    #[inline]
    pub fn to_raw(&self) -> Result<Vec<u8>, codec::Error> {
        bytes::decode_bytes(&mut self.0.as_slice(), false)
    }

    /// Creates a key from encoded bytes vector.
    #[inline]
    pub fn from_encoded(encoded_key: Vec<u8>) -> Key {
        Key(encoded_key)
    }

    /// Creates a key with reserved capacity for timestamp from encoded bytes slice.
    #[inline]
    pub fn from_encoded_slice(encoded_key: &[u8]) -> Key {
        let mut k = Vec::with_capacity(encoded_key.len() + number::U64_SIZE);
        k.extend_from_slice(encoded_key);
        Key(k)
    }

    /// Gets the encoded representation of this key.
    #[inline]
    pub fn as_encoded(&self) -> &Vec<u8> {
        &self.0
    }

    /// Gets and moves the encoded representation of this key.
    #[inline]
    pub fn into_encoded(self) -> Vec<u8> {
        self.0
    }

    /// Creates a new key by appending a `u64` timestamp to this key.
    #[inline]
    pub fn append_ts(mut self, ts: TimeStamp) -> Key {
        self.0.encode_u64_desc(ts.into_inner()).unwrap();
        self
    }

    /// Gets the timestamp contained in this key.
    ///
    /// Preconditions: the caller must ensure this is actually a timestamped
    /// key.
    #[inline]
    pub fn decode_ts(&self) -> Result<TimeStamp, codec::Error> {
        Ok(Self::decode_ts_from(&self.0)?)
    }

    /// Creates a new key by truncating the timestamp from this key.
    ///
    /// Preconditions: the caller must ensure this is actually a timestamped key.
    #[inline]
    pub fn truncate_ts(mut self) -> Result<Key, codec::Error> {
        let len = self.0.len();
        if len < number::U64_SIZE {
            // TODO: IMHO, this should be an assertion failure instead of
            // returning an error. If this happens, it indicates a bug in
            // the caller module, have to make code change to fix it.
            //
            // Even if it passed the length check, it still could be buggy,
            // a better way is to introduce a type `TimestampedKey`, and
            // functions to convert between `TimestampedKey` and `Key`.
            // `TimestampedKey` is in a higher (MVCC) layer, while `Key` is
            // in the core storage engine layer.
            Err(codec::Error::KeyLength)
        } else {
            self.0.truncate(len - number::U64_SIZE);
            Ok(self)
        }
    }

    /// Split a ts encoded key, return the user key and timestamp.
    #[inline]
    pub fn split_on_ts_for(key: &[u8]) -> Result<(&[u8], TimeStamp), codec::Error> {
        if key.len() < number::U64_SIZE {
            Err(codec::Error::KeyLength)
        } else {
            let pos = key.len() - number::U64_SIZE;
            let k = &key[..pos];
            let mut ts = &key[pos..];
            Ok((k, number::decode_u64_desc(&mut ts)?.into()))
        }
    }

    /// Extract the user key from a ts encoded key.
    #[inline]
    pub fn truncate_ts_for(key: &[u8]) -> Result<&[u8], codec::Error> {
        let len = key.len();
        if len < number::U64_SIZE {
            return Err(codec::Error::KeyLength);
        }
        Ok(&key[..key.len() - number::U64_SIZE])
    }

    /// Decode the timestamp from a ts encoded key.
    #[inline]
    pub fn decode_ts_from(key: &[u8]) -> Result<TimeStamp, codec::Error> {
        let len = key.len();
        if len < number::U64_SIZE {
            return Err(codec::Error::KeyLength);
        }
        let mut ts = &key[len - number::U64_SIZE..];
        Ok(number::decode_u64_desc(&mut ts)?.into())
    }

    /// Whether the user key part of a ts encoded key `ts_encoded_key` equals to the encoded
    /// user key `user_key`.
    ///
    /// There is an optimization in this function, which is to compare the last 8 encoded bytes
    /// first before comparing the rest. It is because in TiDB many records are ended with an 8
    /// byte row id and in many situations only this part is different when calling this function.
    //
    // TODO: If the last 8 byte is memory aligned, it would be better.
    #[inline]
    pub fn is_user_key_eq(ts_encoded_key: &[u8], user_key: &[u8]) -> bool {
        let user_key_len = user_key.len();
        if ts_encoded_key.len() != user_key_len + number::U64_SIZE {
            return false;
        }
        if user_key_len >= number::U64_SIZE {
            // We compare last 8 bytes as u64 first, then compare the rest.
            // TODO: Can we just use == to check the left part and right part? `memcmp` might
            //       be smart enough.
            let left = NativeEndian::read_u64(&ts_encoded_key[user_key_len - 8..]);
            let right = NativeEndian::read_u64(&user_key[user_key_len - 8..]);
            if left != right {
                return false;
            }
            ts_encoded_key[..user_key_len - 8] == user_key[..user_key_len - 8]
        } else {
            ts_encoded_key[..user_key_len] == user_key[..]
        }
    }

    /// Returns whether the encoded key is encoded from `raw_key`.
    pub fn is_encoded_from(&self, raw_key: &[u8]) -> bool {
        bytes::is_encoded_from(&self.0, raw_key, false)
    }

    /// TiDB uses the same hash algorithm.
    pub fn gen_hash(&self) -> u64 {
        farmhash::fingerprint64(&self.to_raw().unwrap())
    }
}

impl Clone for Key {
    fn clone(&self) -> Self {
        // default clone implemention use self.len() to reserve capacity
        // for the sake of appending ts, we need to reserve more
        let mut key = Vec::with_capacity(self.0.capacity());
        key.extend_from_slice(&self.0);
        Key(key)
    }
}

impl Debug for Key {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(&hex::encode_upper(&self.0))
    }
}

impl Display for Key {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(&hex::encode_upper(&self.0))
    }
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub enum MutationType {
    Put,
    Delete,
    Lock,
    Insert,
    Other,
}

/// A row mutation.
#[derive(Debug, Clone)]
pub enum Mutation {
    /// Put `Value` into `Key`, overwriting any existing value.
    Put((Key, Value)),
    /// Delete `Key`.
    Delete(Key),
    /// Set a lock on `Key`.
    Lock(Key),
    /// Put `Value` into `Key` if `Key` does not yet exist.
    ///
    /// Returns [`KeyError::AlreadyExists`](kvproto::kvrpcpb::KeyError::AlreadyExists) if the key already exists.
    Insert((Key, Value)),
    /// Check `key` must be not exist.
    ///
    /// Returns [`KeyError::AlreadyExists`](kvproto::kvrpcpb::KeyError::AlreadyExists) if the key already exists.
    CheckNotExists(Key),
}

impl Mutation {
    pub fn key(&self) -> &Key {
        match self {
            Mutation::Put((ref key, _)) => key,
            Mutation::Delete(ref key) => key,
            Mutation::Lock(ref key) => key,
            Mutation::Insert((ref key, _)) => key,
            Mutation::CheckNotExists(ref key) => key,
        }
    }

    pub fn mutation_type(&self) -> MutationType {
        match self {
            Mutation::Put(_) => MutationType::Put,
            Mutation::Delete(_) => MutationType::Delete,
            Mutation::Lock(_) => MutationType::Lock,
            Mutation::Insert(_) => MutationType::Insert,
            _ => MutationType::Other,
        }
    }

    pub fn into_key_value(self) -> (Key, Option<Value>) {
        match self {
            Mutation::Put((key, value)) => (key, Some(value)),
            Mutation::Delete(key) => (key, None),
            Mutation::Lock(key) => (key, None),
            Mutation::Insert((key, value)) => (key, Some(value)),
            Mutation::CheckNotExists(key) => (key, None),
        }
    }

    pub fn should_not_exists(&self) -> bool {
        match self {
            Mutation::Insert(_) | Mutation::CheckNotExists(_) => true,
            _ => false,
        }
    }

    pub fn should_not_write(&self) -> bool {
        match self {
            Mutation::CheckNotExists(_) => true,
            _ => false,
        }
    }
}

impl From<kvrpcpb::Mutation> for Mutation {
    fn from(mut m: kvrpcpb::Mutation) -> Mutation {
        match m.get_op() {
            kvrpcpb::Op::Put => Mutation::Put((Key::from_raw(m.get_key()), m.take_value())),
            kvrpcpb::Op::Del => Mutation::Delete(Key::from_raw(m.get_key())),
            kvrpcpb::Op::Lock => Mutation::Lock(Key::from_raw(m.get_key())),
            kvrpcpb::Op::Insert => Mutation::Insert((Key::from_raw(m.get_key()), m.take_value())),
            kvrpcpb::Op::CheckNotExists => Mutation::CheckNotExists(Key::from_raw(m.get_key())),
            _ => panic!("mismatch Op in prewrite mutations"),
        }
    }
}

#[derive(Default, Debug, Clone, PartialEq)]
pub struct OldValue {
    pub short_value: Option<Value>,
    pub start_ts: TimeStamp,
}

// Returned by MvccTxn when extra_op is set to kvrpcpb::ExtraOp::ReadOldValue.
// key with current ts -> (short value of the prev txn, start ts of the prev txn).
// The value of the map will be None when the mutation is `Insert`.
// MutationType is the type of mutation of the current write.
pub type OldValues = HashMap<Key, (Option<OldValue>, MutationType)>;

// Extra data fields filled by kvrpcpb::ExtraOp.
#[derive(Default, Debug, Clone)]
pub struct TxnExtra {
    old_values: OldValues,
}

impl TxnExtra {
    pub fn add_old_value(
        &mut self,
        key: Key,
        value: Option<OldValue>,
        mutation_type: MutationType,
    ) {
        self.old_values.insert(key, (value, mutation_type));
    }

    pub fn is_empty(&mut self) -> bool {
        self.old_values.is_empty()
    }

    pub fn extend(&mut self, other: &mut Self) {
        self.old_values
            .extend(std::mem::take(&mut other.old_values))
    }

    pub fn mut_old_values(&mut self) -> &mut OldValues {
        &mut self.old_values
    }

    pub fn get_old_values(&self) -> &OldValues {
        &self.old_values
    }
}

#[derive(Debug, Clone)]
pub struct RangeTTL {
    start_key: Key,
    end_key: Key,
    ttl: u64,
}

impl RangeTTL {
    pub fn new(start_key: &[u8], end_key: &[u8], ttl: u64) -> Self {
        RangeTTL {
            start_key: Key::from_raw(start_key),
            end_key: Key::from_raw(end_key),
            ttl,
        }
    }

    fn json_string(&self) -> String {
        format!(
            "{{\"start_key\": \"{}\", \"end_key\":\"{}\", \"ttl\":{}}}",
            self.start_key, self.end_key, self.ttl
        )
    }
}

impl From<pdpb::RangeTtl> for RangeTTL {
    fn from(from: pdpb::RangeTtl) -> Self {
        Self::new(
            from.get_start_key(),
            from.get_end_key(),
            from.get_ttl() * 60000,
        )
    }
}

#[derive(Debug, Clone)]
pub struct RangeExpiry {
    pub start_key: Key,
    pub end_key: Key,
    pub expiry: TimeStamp,
}

impl RangeExpiry {
    fn new(ttl: &RangeTTL, safe_point: TimeStamp, now: TimeStamp) -> Self {
        let safe_point = safe_point.physical();
        let now = now.physical();
        let expiry = if now == 0 {
            safe_point - ttl.ttl
        } else {
            now - ttl.ttl
        };
        let expiry = if expiry > safe_point {
            safe_point
        } else {
            expiry
        };
        RangeExpiry {
            expiry: TimeStamp::compose(expiry, 0),
            start_key: Clone::clone(&ttl.start_key),
            end_key: Clone::clone(&ttl.end_key),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RangeTTLRegistry {
    ttl: Arc<RwLock<RefCell<Arc<Vec<RangeTTL>>>>>,
}

impl RangeTTLRegistry {
    pub fn new() -> Self {
        RangeTTLRegistry {
            ttl: Default::default(),
        }
    }
    pub fn update(&self, ranges: Vec<RangeTTL>) {
        self.ttl.write().unwrap().replace(Arc::new(ranges));
    }

    fn get_ttl(&self) -> Arc<Vec<RangeTTL>> {
        Clone::clone(&self.ttl.read().unwrap().borrow())
    }

    pub fn get(&self, safe_point: TimeStamp, now: TimeStamp) -> Arc<Vec<RangeExpiry>> {
        Arc::new(
            self.get_ttl()
                .iter()
                .map(|ttl| RangeExpiry::new(ttl, safe_point, now))
                .collect(),
        )
    }

    pub fn json_string(&self) -> String {
        let json = self.get_ttl().iter().fold(String::new(), |mut res, ttl| {
            if !res.is_empty() {
                res += ",";
            }
            res += &ttl.json_string();
            res
        });
        format!("[{}]", json)
    }
}

/// We know that nobody is going to mutate RefCell with shared read lock held
unsafe impl Sync for RangeTTLRegistry {}
unsafe impl Send for RangeTTLRegistry {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_user_key_eq() {
        // make a short name to keep format for the test.
        fn eq(a: &[u8], b: &[u8]) -> bool {
            Key::is_user_key_eq(a, b)
        }
        assert_eq!(false, eq(b"", b""));
        assert_eq!(false, eq(b"12345", b""));
        assert_eq!(true, eq(b"12345678", b""));
        assert_eq!(true, eq(b"x12345678", b"x"));
        assert_eq!(false, eq(b"x12345", b"x"));
        // user key len == 3
        assert_eq!(true, eq(b"xyz12345678", b"xyz"));
        assert_eq!(true, eq(b"xyz________", b"xyz"));
        assert_eq!(false, eq(b"xyy12345678", b"xyz"));
        assert_eq!(false, eq(b"yyz12345678", b"xyz"));
        assert_eq!(false, eq(b"xyz12345678", b"xy"));
        assert_eq!(false, eq(b"xyy12345678", b"xy"));
        assert_eq!(false, eq(b"yyz12345678", b"xy"));
        // user key len == 7
        assert_eq!(true, eq(b"abcdefg12345678", b"abcdefg"));
        assert_eq!(true, eq(b"abcdefgzzzzzzzz", b"abcdefg"));
        assert_eq!(false, eq(b"abcdefg12345678", b"abcdef"));
        assert_eq!(false, eq(b"abcdefg12345678", b"bcdefg"));
        assert_eq!(false, eq(b"abcdefv12345678", b"abcdefg"));
        assert_eq!(false, eq(b"vbcdefg12345678", b"abcdefg"));
        assert_eq!(false, eq(b"abccefg12345678", b"abcdefg"));
        // user key len == 8
        assert_eq!(true, eq(b"abcdefgh12345678", b"abcdefgh"));
        assert_eq!(true, eq(b"abcdefghyyyyyyyy", b"abcdefgh"));
        assert_eq!(false, eq(b"abcdefgh12345678", b"abcdefg"));
        assert_eq!(false, eq(b"abcdefgh12345678", b"bcdefgh"));
        assert_eq!(false, eq(b"abcdefgz12345678", b"abcdefgh"));
        assert_eq!(false, eq(b"zbcdefgh12345678", b"abcdefgh"));
        assert_eq!(false, eq(b"abcddfgh12345678", b"abcdefgh"));
        // user key len == 9
        assert_eq!(true, eq(b"abcdefghi12345678", b"abcdefghi"));
        assert_eq!(true, eq(b"abcdefghixxxxxxxx", b"abcdefghi"));
        assert_eq!(false, eq(b"abcdefghi12345678", b"abcdefgh"));
        assert_eq!(false, eq(b"abcdefghi12345678", b"bcdefghi"));
        assert_eq!(false, eq(b"abcdefghy12345678", b"abcdefghi"));
        assert_eq!(false, eq(b"ybcdefghi12345678", b"abcdefghi"));
        assert_eq!(false, eq(b"abcddfghi12345678", b"abcdefghi"));
        // user key len == 11
        assert_eq!(true, eq(b"abcdefghijk87654321", b"abcdefghijk"));
        assert_eq!(true, eq(b"abcdefghijkabcdefgh", b"abcdefghijk"));
        assert_eq!(false, eq(b"abcdefghijk87654321", b"abcdefghij"));
        assert_eq!(false, eq(b"abcdefghijk87654321", b"bcdefghijk"));
        assert_eq!(false, eq(b"abcdefghijx87654321", b"abcdefghijk"));
        assert_eq!(false, eq(b"xbcdefghijk87654321", b"abcdefghijk"));
        assert_eq!(false, eq(b"abxdefghijk87654321", b"abcdefghijk"));
        assert_eq!(false, eq(b"axcdefghijk87654321", b"abcdefghijk"));
        assert_eq!(false, eq(b"abcdeffhijk87654321", b"abcdefghijk"));
    }

    #[test]
    fn test_is_encoded_from() {
        for raw_len in 0..=24 {
            let raw: Vec<u8> = (0..raw_len).collect();
            let encoded = Key::from_raw(&raw);
            assert!(encoded.is_encoded_from(&raw));

            let encoded_len = encoded.as_encoded().len();

            // Should return false if we modify one byte in raw
            for i in 0..raw.len() {
                let mut invalid_raw = raw.clone();
                invalid_raw[i] = raw[i].wrapping_add(1);
                assert!(!encoded.is_encoded_from(&invalid_raw));
            }

            // Should return false if we modify one byte in encoded
            for i in 0..encoded_len {
                let mut invalid_encoded = encoded.clone();
                invalid_encoded.0[i] = encoded.0[i].wrapping_add(1);
                assert!(!invalid_encoded.is_encoded_from(&raw));
            }

            // Should return false if encoded length is not a multiple of 9
            let mut invalid_encoded = encoded.clone();
            invalid_encoded.0.pop();
            assert!(!invalid_encoded.is_encoded_from(&raw));

            // Should return false if encoded has less or more chunks
            let shorter_encoded = Key::from_encoded_slice(&encoded.0[..encoded_len - 9]);
            assert!(!shorter_encoded.is_encoded_from(&raw));
            let mut longer_encoded = encoded.as_encoded().clone();
            longer_encoded.extend(&[0, 0, 0, 0, 0, 0, 0, 0, 0xFF]);
            let longer_encoded = Key::from_encoded(longer_encoded);
            assert!(!longer_encoded.is_encoded_from(&raw));

            // Should return false if raw is longer or shorter
            if !raw.is_empty() {
                let shorter_raw = &raw[..raw.len() - 1];
                assert!(!encoded.is_encoded_from(shorter_raw));
            }
            let mut longer_raw = raw.to_vec();
            longer_raw.push(0);
            assert!(!encoded.is_encoded_from(&longer_raw));
        }
    }
}
