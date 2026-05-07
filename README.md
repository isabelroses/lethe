# lethe

> Never forget your NixOS deployments.

`lethe` records snapshots of NixOS systems. It captures your system's full
closure into a local SQLite database, so you can list, inspect, and diff them
later. This can be useful when looking into past deployments to try and
identify when an issue may first have arise.

Named after the river of oblivion, but it does the opposite.

## Usage

Record the current local system:

```sh
lethe record --local
```

Record a remote machine over SSH:

```sh
lethe record root@my-server
lethe record ssh://my-server:2222
```

By default `lethe` records `/run/current-system`. However, you can pass
`--system-link` to record something else. For example, `/run/booted-system`, or a specific
generation like `/nix/var/nix/profiles/system-626-link`, or any `/nix/store` path.

List what you've recorded:

```sh
lethe machines              # all known machines
lethe deployments my-server # deployments for one machine
lethe show 42               # details of a single deployment
lethe diff 41 42            # diff two deployments (closure + size delta)
lethe diff 41               # diff against the latest deployment of the same machine
```

## Data Storage

The database lives at `$XDG_DATA_HOME/lethe/lethe.db` (typically
`~/.local/share/lethe/lethe.db`). You can change this with `--db <path>` or the
`LETHE_DB` environment variable.

## A little lore

`lethe` is named after [Lethe](https://en.wikipedia.org/wiki/Lethe) one of the
rivers to the underworld from Greek mythology. The word means "forgetting" or
"forgetfulness", which makes the name antithesis. 
