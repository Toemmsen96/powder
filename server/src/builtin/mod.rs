//! Replies the server builds itself, so Powder runs without a game-data pack.
//!
//! A pack, when installed, still answers first; these fill the gaps:
//!   products, product_table   Steep's owned-games entries for Ubisoft Connect (Connect only starts owned games):
//!                             product/add-on/branch ids, and a product configuration written here
//!   sdk                       the Ubisoft services SDK configuration of the game and of Connect: which SDK
//!                             features are on, and the service URL templates on Ubisoft's hostnames
//!   connect_ui                Connect's window and overlay settings, its service addresses, Steep's card
//!   http                      the routes: the game's other start replies (item list version, DLC item
//!                             mappings, event types, challenges), and Connect's cached UI settings left alone
//!
//! Only interface facts (ids, hostnames, URL templates, feature names, the item list's fingerprint) and values
//! written for Powder; no copies of Ubisoft's texts, images or catalogues. The bodies are byte for byte what
//! the Python server sends (JSON as `json.dumps` writes it): the game and Connect parse them.

pub mod connect_ui;
pub mod http;
pub mod product_table;
pub mod products;
pub mod sdk;
