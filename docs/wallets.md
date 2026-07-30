# ACCOUNTS

How to set up an account to trade from.

An account here is an **encrypted keystore file** — your private key, encrypted
with a password you type each time you unlock it. That is the only kind of
account the bot has. There is no way to configure a plain private key or a seed
phrase.

## Make one in the app

Press **W** at any point, or pick from the account list on the way in.

Three options, and all three end at the same place — a password-encrypted
keystore in `~/.foundry/keystores`:

```
＋ Create a new account       generates a fresh key
＋ Import a private key       paste a key you already have
＋ Import a seed phrase       derives the key at an index you choose
```

The bot writes those files itself rather than shelling out to another tool: no
CLI to install, no output to parse, and no password sitting in your shell
history.

**Importing a phrase does not store it.** The key is derived, encrypted and
written; the phrase is not saved. If you want a second account from the same
phrase, import it again at a different index.

Name the account when asked. That name is what you pick from the list later, and
the one you used last sits at the top.

## Or use Foundry

The keystore directory is shared with Foundry deliberately, so anything `cast`
makes shows up in the bot and the other way round. Use it if you already have it
— it is not required.

```
curl -L https://foundry.paradigm.xyz | bash
foundryup
```

**A new key:**

```
cast wallet new ~/.foundry/keystores --account robin
```

**A key you already have:**

```
cast wallet import robin --interactive
```

`--interactive` prompts for the key so it never lands in your history.

**Check what is there:**

```
cast wallet list
```

Those names are the names the bot shows.

## Nothing to configure

There is no step where you write the account into a file. The bot lists the
keystores it finds on disk — make one and it is in the list on the next run.

`config.json` has an `accounts` block, but the app fills it in. It is a record of
what you have, not something to author. See **Config**.

## Solana

Same flow, same directory, same **W** key.

Solana keystores deliberately use the Ethereum keystore format. Solana's own CLI
writes keys as plaintext JSON arrays, and an encrypted file at rest is worth more
than matching their convention.

Solana accounts derive on the Phantom-compatible path `m/44'/501'/n'/0'`, so an
address the bot derives from a phrase matches what Phantom shows for the same
phrase and index. The two chains derive **differently** from one phrase —
Ethereum uses `m/44'/60'/0'/0/n` — so the same seed gives different addresses on
each side. Expected, not a bug.

## Afterwards

```
cast wallet address --account robin     # show the address
cast wallet change-password robin       # rotate the password
cast wallet remove robin                # delete the keystore
```

Deleting a keystore removes the account from the list. It does not touch anything
on chain.

## Losing the password

There is no recovery. The keystore is encrypted with it, and nobody — including
us — can open the file without it. If the key came from a phrase you still have,
import it again and set a new password. If it did not, that key is gone.

Back up the keystore files. They are useless to anyone without the password,
which is what makes them safe to copy somewhere.

## What the bot will not do

- It never sends your key anywhere. Signing happens on your machine.
- It never trades on its own. `b`, `s` and `x` are the only keys that submit a
  transaction.
- Copy modes only pre-fill a size. They do not trade.
- It never writes a password or a key to a log. Session logs record addresses and
  amounts; skim one before attaching it to a bug report anyway.
