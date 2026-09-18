//! Chainlink-compatible proof construction; secret scalar operations use k256.
//! BigUint is used only for public field/hash/witness arithmetic, never private scalar math.
//! This adapter needs external review before production. No reused/deterministic test nonce.
use crate::abi::VrfProof;
use alloy_primitives::{Address, U256, keccak256};
use anyhow::{Result, bail, ensure};
use k256::{
    AffinePoint, EncodedPoint, ProjectivePoint, Scalar, SecretKey,
    elliptic_curve::{
        ops::Reduce,
        rand_core::OsRng,
        sec1::{FromEncodedPoint, ToEncodedPoint},
    },
};
use num_bigint::BigUint;
use num_traits::{One, Zero};
use std::path::Path;
use zeroize::Zeroizing;

pub fn read_key(path: &Path) -> Result<SecretKey> {
    ensure!(
        std::fs::metadata(path)?.len() <= 128,
        "Key file exceeds expected size"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            std::fs::metadata(path)?.permissions().mode() & 0o077 == 0,
            "Key file must not be readable by group/others"
        );
    }
    let text = Zeroizing::new(std::fs::read_to_string(path)?);
    let bytes = Zeroizing::new(
        hex::decode(text.trim().strip_prefix("0x").unwrap_or(text.trim()))
            .map_err(|_| anyhow::anyhow!("Invalid key encoding"))?,
    );
    ensure!(bytes.len() == 32, "Key must contain 32 bytes");
    SecretKey::from_slice(&bytes).map_err(|_| anyhow::anyhow!("Invalid secp256k1 private key"))
}
fn prime() -> BigUint {
    BigUint::parse_bytes(
        b"FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEFFFFFC2F",
        16,
    )
    .unwrap()
}
fn word(x: &BigUint) -> [u8; 32] {
    let b = x.to_bytes_be();
    let mut out = [0u8; 32];
    out[32 - b.len()..].copy_from_slice(&b);
    out
}
fn pair(point: ProjectivePoint) -> [U256; 2] {
    let p = point.to_affine().to_encoded_point(false);
    [
        U256::from_be_slice(p.x().expect("nonidentity")),
        U256::from_be_slice(p.y().expect("nonidentity")),
    ]
}
pub fn public_key(key: &SecretKey) -> [U256; 2] {
    pair(ProjectivePoint::GENERATOR * key.to_nonzero_scalar().as_ref())
}
pub fn address(key: &SecretKey) -> Address {
    let p = public_key(key);
    let mut b = Vec::new();
    for x in p {
        b.extend_from_slice(&x.to_be_bytes::<32>());
    }
    Address::from_slice(&keccak256(b)[12..])
}
fn append(out: &mut Vec<u8>, p: [U256; 2]) {
    for x in p {
        out.extend_from_slice(&x.to_be_bytes::<32>());
    }
}
fn field_hash(bytes: &[u8], p: &BigUint) -> BigUint {
    let mut x = BigUint::from_bytes_be(keccak256(bytes).as_slice());
    while &x >= p {
        x = BigUint::from_bytes_be(keccak256(word(&x)).as_slice());
    }
    x
}
fn hash_to_curve(pk: [U256; 2], seed: U256) -> Result<ProjectivePoint> {
    let p = prime();
    let exp = (&p + BigUint::one()) >> 2;
    let mut input = U256::from(1).to_be_bytes::<32>().to_vec();
    append(&mut input, pk);
    input.extend_from_slice(&seed.to_be_bytes::<32>());
    let mut x = field_hash(&input, &p);
    for _ in 0..256 {
        let y2 = ((&x * &x % &p) * &x + BigUint::from(7u8)) % &p;
        let mut y = y2.modpow(&exp, &p);
        if &y * &y % &p == y2 {
            if (&y & BigUint::one()) == BigUint::one() {
                y = &p - y;
            }
            let mut encoded = vec![4u8];
            encoded.extend_from_slice(&word(&x));
            encoded.extend_from_slice(&word(&y));
            let ep = EncodedPoint::from_bytes(encoded)?;
            let a = Option::<AffinePoint>::from(AffinePoint::from_encoded_point(&ep))
                .ok_or_else(|| anyhow::anyhow!("Invalid public hash-to-curve point"))?;
            return Ok(ProjectivePoint::from(a));
        }
        x = field_hash(&word(&x), &p);
    }
    bail!("Hash-to-curve work bound exceeded")
}
pub fn prove(seed: U256, key: &SecretKey) -> Result<VrfProof> {
    let sk = key.to_nonzero_scalar();
    let pk = public_key(key);
    let h = hash_to_curve(pk, seed)?;
    let gamma = h * sk.as_ref();
    let p = prime();
    for _ in 0..32 {
        let nonce = SecretKey::random(&mut OsRng);
        let k = nonce.to_nonzero_scalar();
        let u = ProjectivePoint::GENERATOR * k.as_ref();
        let v = h * k.as_ref();
        let mut up = Vec::new();
        append(&mut up, pair(u));
        let u_address = Address::from_slice(&keccak256(up)[12..]);
        if u_address.is_zero() {
            continue;
        }
        let mut c_input = U256::from(2).to_be_bytes::<32>().to_vec();
        for point in [pair(h), pk, pair(gamma), pair(v)] {
            append(&mut c_input, point);
        }
        c_input.extend_from_slice(u_address.as_slice());
        let c_bytes = keccak256(c_input);
        let c_scalar = <Scalar as Reduce<k256::U256>>::reduce_bytes(&(*c_bytes).into());
        let s = *k.as_ref() - c_scalar * sk.as_ref();
        if bool::from(c_scalar.is_zero()) || bool::from(s.is_zero()) {
            continue;
        }
        let cg = pair(gamma * c_scalar);
        let sh = pair(h * s);
        if cg[0] == sh[0] {
            continue;
        }
        let x1 = BigUint::from_bytes_be(&cg[0].to_be_bytes::<32>());
        let x2 = BigUint::from_bytes_be(&sh[0].to_be_bytes::<32>());
        let lz = (&x2 + &p - &x1) % &p;
        let dx = &lz * &lz % &p;
        let dy = &dx * &lz % &p;
        let z = if dx == dy { dx } else { dx * dy % &p };
        ensure!(!z.is_zero(), "Invalid public inverse witness");
        let inv = z.modpow(&(&p - BigUint::from(2u8)), &p);
        return Ok(VrfProof {
            pk,
            gamma: pair(gamma),
            c: U256::from_be_bytes(*c_bytes),
            s: U256::from_be_slice(&s.to_bytes()),
            seed,
            uWitness: u_address,
            cGammaWitness: cg,
            sHashWitness: sh,
            zInv: U256::from_be_bytes(word(&inv)),
        });
    }
    bail!("Unable to generate nondegenerate VRF proof")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn proof_nonces_do_not_change_output_point() {
        let key = SecretKey::from_slice(&U256::from(123456789u64).to_be_bytes::<32>()).unwrap();
        let a = prove(U256::from(123), &key).unwrap();
        let b = prove(U256::from(123), &key).unwrap();
        assert_eq!(a.gamma, b.gamma);
        assert_ne!(a.c, b.c);
        assert_eq!(a.pk, public_key(&key));
    }
}
