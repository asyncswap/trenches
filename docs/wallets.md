# ACCOUNTS

How to set up an account to trade from.

An account here is an **encrypted keystore file** — your private key, encrypted
with a password you type each time you unlock it. That is the only kind of
account the bot has. There is no way to configure a plain private key or a seed
phrase.

## Make one in the app

Press **W** at any point, or pick from the account list on the way in.

Three options, and all three end at the same place — a password-encrypted
keystore in `~/.config/trenches/keystores`:

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

## Where they live

Two directories, and the bot reads both:

```
~/.config/trenches/keystores   accounts made here
~/.foundry/keystores           accounts made with Foundry
```

Anything `cast` makes shows up in the bot, and the other way round — the format
is the standard Web3 Secret Storage one. Foundry is not required, just
compatible.

The split matters when you reach for `cast`, because `cast` looks in the
Foundry directory unless told otherwise. An account made in the bot needs the
directory naming, every time:

```
cast wallet list --dir ~/.config/trenches/keystores
```

Note `list` takes `--dir` while the commands below take `-k`. Not our choice.

There is nothing to configure afterwards: the bot lists the keystores it finds
on disk. Make one and it is in the list on the next run.

## Solana

Same flow, same directory, same **W** key.

Phrases derive on the Phantom-compatible path, so an address derived here
matches what Phantom shows for the same phrase and index. The two chains
derive **differently** from one phrase, so the same seed gives different
addresses on each side. Expected, not a bug.

## Changing the password

The bot has no key management screen and is not going to grow one — key
handling is exactly the place where a hand-rolled tool earns nothing and can
cost everything. Use `cast`, which does this properly and is already installed
if you have Foundry.

```
cast wallet change-password trench -k ~/.config/trenches/keystores
```

It asks for the current password, then the new one twice, and rewrites the file
in place. The address does not change, and neither does the name — the account
appears in the bot exactly as before, opening with the new password.

Do this somewhere the file is backed up first. A change-password that is
interrupted at the wrong moment is the one operation here that can leave you
without either password working.

## Exporting the private key

```
cast wallet decrypt-keystore trench -k ~/.config/trenches/keystores
```

It asks for the password and prints the private key to the terminal.

Be deliberate about this one. A key on screen is a key in your scrollback, in
your terminal's session log if it keeps one, and in any screen recording that
happens to be running. It is also the only form in which the key is not
protected by a password. Close the terminal afterwards, and do not paste the
output anywhere you would not paste the wallet itself.

Legitimate reasons: importing into another wallet, moving to a hardware signer,
or keeping a cold backup. If you are exporting because someone asked you to,
stop.

## Coming later: unlocking with Touch ID

On a Mac with Touch ID, the password step could be a fingerprint instead —
the key stays encrypted at rest and the Secure Enclave releases it, so nothing
about the file's protection changes and there is no password to type into a
terminal a dozen times a session.

Not built yet. Until it is, the password is the only way in, which is why the
section below matters.

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
