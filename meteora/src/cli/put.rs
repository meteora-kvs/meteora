use clap::ArgMatches;

use meteora_client::kv::client::KVClient;

use crate::log::set_logger;

pub fn run_put_cli(matches: &ArgMatches) -> Result<(), std::io::Error> {
    set_logger();

    let address = matches.value_of("ADDRESS").unwrap();
    let key = matches.value_of("KEY").unwrap();
    let value = matches.value_of("VALUE").unwrap();

    let mut kv_client = KVClient::new(address);

    kv_client.put(key.as_bytes().to_vec(), value.as_bytes().to_vec())
}
