use bstr::BString;

const HQ_ENV_PREFIX: &str = "HQ_";

pub fn create_hq_env(name: &str) -> BString {
    format!("{}{}", HQ_ENV_PREFIX, name).into()
}

pub fn is_hq_env(name: &BString) -> bool {
    name.starts_with(HQ_ENV_PREFIX.as_bytes())
}
