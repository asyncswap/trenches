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

**Importing a phrase does not store it.** The key is derived, encrypted and
written; the phrase is not saved. If you want a second account from the same
phrase, import it again at a different index.

Name the account when asked. That name is what you pick from the list later, and
the one you used last sits at the top.

## Or use Foundry

The keystore directory is shared with Foundry, so anything `cast` makes shows
up in the bot and the other way round — `cast wallet list` shows the same
names. Not required, just compatible.

There is nothing to configure afterwards: the bot lists the keystores it finds
on disk. Make one and it is in the list on the next run.

## Solana

Same flow, same directory, same **W** key.

Phrases derive on the Phantom-compatible path, so an address derived here
matches what Phantom shows for the same phrase and index. The two chains
derive **differently** from one phrase, so the same seed gives different
addresses on each side. Expected, not a bug.

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
