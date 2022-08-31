# solana-deployer

Deploy your Solana programs during high load.

Fork from [solana-deployer by acheroncrypto](https://github.com/acheroncrypto/solana-deployer).

## Installation

```sh
cargo install --git https://github.com/lucasig11/solana-deployer
```

## Run

Generate the configuration template.
```sh
solana-deployer gen-config -o deploy.toml
```
Tweak it.
```sh
$EDITOR deploy.toml
```
Run the deployer.
```sh
solana-deployer [-c CONFIG_PATH]
```

