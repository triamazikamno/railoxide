use super::*;

pub fn parse_unshield_amount(input: &str, decimals: Option<u8>) -> Result<U256> {
    let input = input.trim();
    if input.is_empty() {
        return Err(eyre!("amount is required"));
    }

    if let Some(decimals) = decimals {
        parse_scaled_amount(input, decimals)
    } else {
        if !input.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(eyre!("unknown token amounts must be raw integer units"));
        }
        U256::from_str_radix(input, 10).wrap_err("invalid raw amount")
    }
}

pub fn parse_send_amount(input: &str, decimals: Option<u8>) -> Result<U256> {
    parse_unshield_amount(input, decimals)
}

pub fn parse_railgun_recipient(input: &str) -> Result<AddressData> {
    let input = input.trim();
    if input.is_empty() {
        return Err(eyre!("recipient 0zk address is required"));
    }
    let railgun_addr = RailgunAddress::from(input);
    AddressData::try_from(&railgun_addr).wrap_err("invalid recipient 0zk address")
}

#[must_use]
pub(crate) const fn wrapped_native_token_for_chain(chain_id: u64) -> Option<Address> {
    match chain_id {
        1 => Some(address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")),
        56 => Some(address!("0xbb4CdB9CBd36B01bD1cBaEBF2De08d9173bc095c")),
        137 => Some(address!("0x0d500B1d8E8eF31E21C99d1Db9A6444d3ADf1270")),
        42161 => Some(address!("0x82aF49447D8a07e3bd95BD0d56f35241523fBab1")),
        _ => None,
    }
}

#[must_use]
pub fn is_wrapped_native_token(chain_id: u64, token: Address) -> bool {
    wrapped_native_token_for_chain(chain_id).is_some_and(|wrapped| wrapped == token)
}

fn parse_scaled_amount(input: &str, decimals: u8) -> Result<U256> {
    let (whole, fractional) = input
        .split_once('.')
        .map_or((input, ""), |(whole, fractional)| (whole, fractional));
    if whole.is_empty() && fractional.is_empty() {
        return Err(eyre!("amount is required"));
    }
    if !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fractional.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(eyre!("amount must contain only decimal digits"));
    }
    if fractional.len() > usize::from(decimals) {
        return Err(eyre!("amount has too many decimal places"));
    }

    let whole_value = if whole.is_empty() {
        U256::ZERO
    } else {
        U256::from_str_radix(whole, 10).wrap_err("invalid whole amount")?
    };
    let scale = uint!(10_U256)
        .checked_pow(U256::from(decimals))
        .ok_or_else(|| eyre!("token precision exceeds the supported amount range"))?;
    let fractional_value = if decimals == 0 || fractional.is_empty() {
        U256::ZERO
    } else {
        let mut padded = fractional.to_owned();
        padded.extend(std::iter::repeat_n(
            '0',
            usize::from(decimals) - fractional.len(),
        ));
        U256::from_str_radix(&padded, 10).wrap_err("invalid fractional amount")?
    };

    whole_value
        .checked_mul(scale)
        .and_then(|whole| whole.checked_add(fractional_value))
        .ok_or_else(|| eyre!("amount exceeds the supported range"))
}
