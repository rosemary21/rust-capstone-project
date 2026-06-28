#![allow(unused)]
use bitcoin::hex::DisplayHex;
use bitcoincore_rpc::bitcoin::{Amount, Network};
use bitcoincore_rpc::{Auth, Client, RpcApi};
use serde::Deserialize;
use serde_json::json;
use std::fs::File;
use std::io::Write;

// Node access params
const RPC_URL: &str = "http://127.0.0.1:18443"; // Default regtest RPC port
const RPC_USER: &str = "alice";
const RPC_PASS: &str = "password";

fn auth() -> Auth {
    Auth::UserPass(RPC_USER.to_owned(), RPC_PASS.to_owned())
}

fn send(rpc: &Client, addr: &str) -> bitcoincore_rpc::Result<String> {
    let args = [
        json!([{addr : 100 }]), // recipient address
        json!(null),            // conf target
        json!(null),            // estimate mode
        json!(null),            // fee rate in sats/vb
        json!(null),            // Empty option object
    ];

    #[derive(Deserialize)]
    struct SendResult {
        complete: bool,
        txid: String,
    }
    let send_result = rpc.call::<SendResult>("send", &args)?;
    assert!(send_result.complete);
    Ok(send_result.txid)
}

/// Build a wallet-scoped RPC client for the given wallet name.
fn wallet_client(name: &str) -> bitcoincore_rpc::Result<Client> {
    Client::new(&format!("{}/wallet/{}", RPC_URL, name), auth())
}

/// Try to create the wallet; if it already exists on disk, load it instead;
/// if it is already loaded by the node, silently continue.
fn create_or_load_wallet(rpc: &Client, name: &str) -> bitcoincore_rpc::Result<()> {
    match rpc.create_wallet(name, None, None, None, None) {
        Ok(_) => println!("Wallet '{}' created.", name),
        Err(_) => match rpc.load_wallet(name) {
            Ok(_) => println!("Wallet '{}' loaded.", name),
            Err(e) => {
                if !e.to_string().contains("already loaded") {
                    return Err(e);
                }
                println!("Wallet '{}' already loaded.", name);
            }
        },
    }
    Ok(())
}

fn main() -> bitcoincore_rpc::Result<()> {
    // Connect to Bitcoin Core RPC
    let rpc = Client::new(RPC_URL, auth())?;

    // Get blockchain info
    let blockchain_info = rpc.get_blockchain_info()?;
    println!("Blockchain Info: {:?}", blockchain_info);

    // Create/Load the wallets, named 'Miner' and 'Trader'. Have logic to optionally create/load them if they do not exist or not loaded already.
    create_or_load_wallet(&rpc, "Miner")?;
    create_or_load_wallet(&rpc, "Trader")?;

    // Create wallet-scoped RPC clients so subsequent calls target the right wallet
    let miner_rpc = wallet_client("Miner")?;
    let trader_rpc = wallet_client("Trader")?;

    let mining_addr = miner_rpc
        .get_new_address(Some("Mining Reward"), None)?
        .require_network(Network::Regtest)
        .expect("address should be on regtest");

    miner_rpc.generate_to_address(101, &mining_addr)?;
    println!("Mined 101 blocks to fund Miner.");

    let miner_balance = miner_rpc.get_balance(None, None)?;
    println!("Miner balance: {} BTC", miner_balance.to_btc());

    // Load Trader wallet and generate a new address
    let trader_addr = trader_rpc
        .get_new_address(Some("Received"), None)?
        .require_network(Network::Regtest)
        .expect("address should be on regtest");
    println!("Trader address: {}", trader_addr);

    // Send 20 BTC from Miner to Trader
    let txid = miner_rpc.send_to_address(
        &trader_addr,
        Amount::from_sat(2_000_000_000), // 20 BTC = 2,000,000,000 satoshis
        None,
        None,
        None,
        None,
        None,
        None,
    )?;
    println!("Sent 20 BTC. txid={}", txid);

    // Check transaction in mempool
    let mempool_entry =
        rpc.call::<serde_json::Value>("getmempoolentry", &[json!(txid.to_string())])?;
    println!(
        "Mempool entry:\n{}",
        serde_json::to_string_pretty(&mempool_entry).unwrap()
    );

    // Mine 1 block to confirm the transaction
    miner_rpc.generate_to_address(1, &mining_addr)?;
    println!("Transaction confirmed.");

    // Extract all required transaction details
    //
    // gettransaction gives us the wallet-level view: blockheight, blockhash, fee
    let tx_info = miner_rpc.get_transaction(&txid, None)?;
    let block_height = tx_info.info.blockheight.expect("tx should be confirmed");
    let block_hash = tx_info.info.blockhash.expect("tx should have block hash");
    // Fee is reported as a negative amount from the sender's perspective
    let fee_btc = tx_info.fee.expect("tx should report a fee").to_btc();

    // Decode the full transaction to inspect its inputs and outputs
    let raw_tx = rpc.get_raw_transaction_info(&txid, Some(&block_hash))?;

    let vin0 = &raw_tx.vin[0];
    let prev_txid = vin0.txid.expect("vin should reference a previous tx");
    let prev_vout_idx = vin0.vout.expect("vin should have a vout index") as usize;

    let prev_tx = rpc.get_raw_transaction_info(&prev_txid, None)?;
    let prev_out = &prev_tx.vout[prev_vout_idx];
    // Prefer the singular `address` field (Bitcoin Core 22+); fall back to the
    // legacy `addresses` array for older node versions.
    let input_addr = prev_out
        .script_pub_key
        .address
        .as_ref()
        .or_else(|| prev_out.script_pub_key.addresses.first())
        .expect("input UTXO should have an address")
        .clone()
        .assume_checked()
        .to_string();
    let input_amount_btc = prev_out.value.to_btc();

    // Classify the two vouts: Trader's payment vs. Miner's change
    let trader_addr_str = trader_addr.to_string();
    let mut trader_out_addr = String::new();
    let mut trader_out_amount = 0.0_f64;
    let mut miner_change_addr = String::new();
    let mut miner_change_amount = 0.0_f64;

    for vout in &raw_tx.vout {
        let addr_opt = vout
            .script_pub_key
            .address
            .as_ref()
            .or_else(|| vout.script_pub_key.addresses.first());

        if let Some(addr) = addr_opt {
            let addr_str = addr.clone().assume_checked().to_string();
            if addr_str == trader_addr_str {
                trader_out_addr = addr_str;
                trader_out_amount = vout.value.to_btc();
            } else {
                miner_change_addr = addr_str;
                miner_change_amount = vout.value.to_btc();
            }
        }
    }

    // Write the data to ../out.txt in the specified format given in readme.md
    // One value per line:
    //   txid | miner input address | miner input amount | trader address |
    //   trader amount | miner change address | miner change amount |
    //   fee | block height | block hash
    let mut out = File::create("../out.txt")?;
    writeln!(out, "{}", txid)?;
    writeln!(out, "{}", input_addr)?;
    writeln!(out, "{}", input_amount_btc)?;
    writeln!(out, "{}", trader_out_addr)?;
    writeln!(out, "{}", trader_out_amount)?;
    writeln!(out, "{}", miner_change_addr)?;
    writeln!(out, "{}", miner_change_amount)?;
    writeln!(out, "{}", fee_btc)?;
    writeln!(out, "{}", block_height)?;
    writeln!(out, "{}", block_hash)?;

    println!("Results written to ../out.txt");
    Ok(())
}
