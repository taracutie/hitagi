use serde::{de::DeserializeOwned, Serialize};

pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, String> {
    bincode::serde::encode_to_vec(value, bincode::config::standard()).map_err(|err| err.to_string())
}

pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, String> {
    let (value, read): (T, usize) =
        bincode::serde::decode_from_slice(bytes, bincode::config::standard())
            .map_err(|err| err.to_string())?;
    if read != bytes.len() {
        return Err(format!(
            "decoded {read} bytes from {} byte blob",
            bytes.len()
        ));
    }
    Ok(value)
}
