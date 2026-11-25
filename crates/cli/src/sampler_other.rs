//! `--samples` away from Windows: the sampler of the program's own threads is
//! written for Windows so far, so here it takes nothing and says so.
pub struct Sampler;

impl Sampler {
    pub fn start() -> Self {
        Self
    }

    pub fn finish(self, _path: &str) -> std::io::Result<usize> {
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "--samples is not built for this system yet"))
    }
}
