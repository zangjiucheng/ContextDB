mod counting {
    type ExactMode = contextdb_core::Counting;
    mod cases {
        include!("common/p0_cases.rs");
    }
}
mod full {
    type ExactMode = contextdb_core::FullRecompute;
    mod cases {
        include!("common/p0_cases.rs");
    }
}
