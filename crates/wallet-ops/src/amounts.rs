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
        9745 => Some(address!("0x6100E367285b01F48D07953803A2d8dCA5D19873")),
        5000 => Some(address!("0x78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8")),
        42793 => Some(address!("0xc9B53AB2679f573e480d01e0f49e2B5CFB7a3EAb")),
        146 => Some(address!("0x039e2fB66102314Ce7b64Ce5Ce3E5183bc94aD38")),
        // OP-stack chains share the predeploy WETH address.
        10 | 130 | 480 | 4326 | 8453 | 57073 => {
            Some(address!("0x4200000000000000000000000000000000000006"))
        }
        43114 => Some(address!("0xB31f66AA3C1e785363F0875A1B74E27b85FD66c7")),
        100 => Some(address!("0xe91D153E0b41518A2Ce8Dd3D7944Fa863463a97d")),
        81457 => Some(address!("0x4300000000000000000000000000000000000004")),
        59144 => Some(address!("0xe5D7C2a44FfDDf6b295A15c148167daaAf5Cf34f")),
        747_474 => Some(address!("0xEE7D8BCFb72bC1880D0Cf19822eB0A2e6577aB62")),
        30 => Some(address!("0x542fDA317318eBF1d3DEAf76E0b632741A7e677d")),
        25 => Some(address!("0x5C7F8A570d578ED84E63fdFA7b1eE72dEae1AE23")),
        80094 => Some(address!("0x6969696969696969696969696969696969696969")),
        999 => Some(address!("0x5555555555555555555555555555555555555555")),
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
