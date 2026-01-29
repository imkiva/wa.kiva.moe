use clap::Parser;

#[derive(Debug, Parser, Clone)]
pub struct AppOpts {
  #[clap(short = 'l', long, env, default_value = "0.0.0.0:9980")]
  pub listen: String,
}

pub mod video_gw;
