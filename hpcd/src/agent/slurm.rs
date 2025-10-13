use serde::{Deserialize, Serialize};

pub const DETERMINE_SLURM_VERSION_CMD: &str = r#"(scontrol --version 2>/dev/null || srun --version 2>/dev/null || sinfo --version 2>/dev/null || squeue --version 2>/dev/null; )"#;
