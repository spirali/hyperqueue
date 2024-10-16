use bstr::BString;

const HQ_ENV_PREFIX: &str = "HQ_";

macro_rules! create_hq_env {
    ($name: literal) => {
        concat!("HQ_", $name)
    };
}

pub fn is_hq_env(name: &BString) -> bool {
    name.starts_with(HQ_ENV_PREFIX.as_bytes())
}

/// Known environment variables
pub const HQ_JOB_ID: &str = create_hq_env!("JOB_ID");
pub const HQ_TASK_ID: &str = create_hq_env!("TASK_ID");
pub const HQ_INSTANCE_ID: &str = create_hq_env!("INSTANCE_ID");
pub const HQ_SUBMIT_DIR: &str = create_hq_env!("SUBMIT_DIR");
pub const HQ_ENTRY: &str = create_hq_env!("ENTRY");
pub const HQ_PIN: &str = create_hq_env!("PIN");
pub const HQ_TASK_DIR: &str = create_hq_env!("TASK_DIR");
pub const HQ_ERROR_FILENAME: &str = create_hq_env!("ERROR_FILENAME");
pub const HQ_CPUS: &str = create_hq_env!("CPUS");
pub const HQ_NODE_FILE: &str = create_hq_env!("NODE_FILE");
pub const HQ_HOST_FILE: &str = create_hq_env!("HOST_FILE");
pub const HQ_NUM_NODES: &str = create_hq_env!("NUM_NODES");
pub const HQ_DATA_ACCESS: &str = create_hq_env!("DATA_ACCESS");
