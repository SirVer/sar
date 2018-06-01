// Heavily lifted implementation from https://github.com/nlitsme/vimdecrypt.

#[macro_use]
extern crate failure;

pub type Result<T> = ::std::result::Result<T, failure::Error>;

#[derive(Debug)]
enum CryptMethod { Zip, Blowfish, Blowfish2 }

impl CryptMethod {
    fn from_header(data: &[u8]) -> Result<Self> {
        match &data[0..12] {
            b"VimCrypt~01!" => Ok(CryptMethod::Zip),
            b"VimCrypt~02!" => Ok(CryptMethod::Blowfish),
            b"VimCrypt~03!" => Ok(CryptMethod::Blowfish2),
            _ => bail!("Unknown VimCrypt header."),
        }
    }
}

fn make_crc_table(seed: u32) -> Vec<u32> {
    fn calc_entry(mut v: u32, seed: u32) -> u32 {
        for _ in 0..8 {
            v = (v>>1) ^ (if v & 1 != 0 { seed } else {0})
        }
        v
    }

    (0..256).map(|b| calc_entry(b, seed)).collect()
}

pub fn zip_decrypt(data: &[u8], password: &str) -> Result<Vec<u8>> {
    let crc_table = make_crc_table(0xedb88320);

    let crc32 = |crc, byte: u8| crc_table[((crc^(byte as u32))&0xff) as usize] ^ (crc >> 8);
    let mut keys = [0x12345678u32, 0x23456789u32, 0x34567890u32 ];
    let update_keys = |keys: &mut[u32], byte| {
        keys[0] = crc32(keys[0], byte);
        keys[1] = ((keys[1] + (keys[0]&0xFF)) * 134775813 + 1)&0xFFFFFFFF;
        keys[2] = crc32(keys[2], (keys[1]>>24) as u8);
    };

    for c in password.chars() {
        update_keys(&mut keys, c as u8);
    }

    let mut plain_text = Vec::with_capacity(data.len());
    for b in data {
        let xor = (keys[2] | 2)&0xFFFF;
        let xor = ((xor * (xor^1))>>8) & 0xFF;
        let b = b ^ (xor as u8);
        plain_text.push(b);
        update_keys(&mut keys, b);
    }
    Ok(plain_text)
}


pub fn decrypt(data: &[u8], password: &str) -> Result<Vec<u8>> {
    let method = CryptMethod::from_header(&data[0..12])?;
    let data = match method {
        CryptMethod::Zip => zip_decrypt(&data[12..], password)?,
        _ => unimplemented!(),
    };
    Ok(data)
}
