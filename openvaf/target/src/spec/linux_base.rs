use crate::spec::{LinkerFlavor, TargetOptions};

pub fn opts() -> TargetOptions {
    let mut opts = TargetOptions::default();
    let link_args = opts.pre_link_args.entry(LinkerFlavor::Ld).or_default();
    opts
}
