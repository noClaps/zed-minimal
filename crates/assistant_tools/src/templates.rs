use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "src/templates"]
#[include = "*.hbs"]
struct Assets;
