# lethe

> Never forget your NixOS deployments.

`lethe` records snapshots of NixOS systems. It captures your system's full
closure into a local SQLite database, so you can list, inspect, and diff them
later. This can be useful when looking into past deployments to try and
identify when an issue may first have arise.

Named after the river of oblivion, but it does the opposite.

## Usage

Record the machine you are on:

```sh
lethe record
```

Record a remote machine over SSH:

```sh
lethe record root@my-server
lethe record ssh-ng://my-server:2222
lethe record ssh://my-server:2222
```

Bare hostnames use the `ssh-ng` Nix store protocol by default. Specify
`ssh://` to use the legacy SSH store protocol instead. Explicit ports are used
for both the SSH connection and the Nix store connection; existing `NIX_SSHOPTS`
are preserved for the latter.

By default `lethe` records `/run/current-system`. However, you can pass
`--system-link` to record something else. For example, `/run/booted-system`, or a specific
generation like `/nix/var/nix/profiles/system-626-link`, or any `/nix/store` path.

List what you've recorded:

```sh
lethe machines              # all known machines
lethe deployments my-server # deployments for one machine
lethe show my-server        # details of the latest deployment of a machine
lethe show my-server 3      # details of one deployment
lethe diff my-server        # diff the last two deployments (closure + size delta)
lethe diff my-server 3      # diff deployment 3 against the latest
lethe diff my-server 3 5    # diff two deployments
lethe diff my-server /nix/store/...-nixos-system-foo 5  # numbers and toplevel paths are interchangeable
```

## Data Storage

The database lives at `$XDG_DATA_HOME/lethe/lethe.db` (typically
`~/.local/share/lethe/lethe.db`). You can change this with `--db <path>` or the
`LETHE_DB` environment variable.

## Syncing between devices

If you deploy from more than one machine, run a sync server on one always-on
host and let every orchestrator sync with it. Each deployment carries a stable
uuid, so syncing is safe to repeat and never duplicates anything. A machine's
deployments are numbered by when they were made, so a pulled deployment
slots in where it happened and every device ends up with the same numbers;
the numbers `show` and `diff` take can therefore shift after a sync.

On the server (or use the NixOS module, below):

```sh
lethe serve --listen 0.0.0.0:7374 --token-file /run/secrets/lethe-token
```

On each orchestrator:

```sh
export LETHE_SYNC_SERVER=http://freyja:7374
export LETHE_SYNC_TOKEN_FILE=~/.config/lethe/token
lethe sync   # pushes what you recorded, pulls everyone else's
```

`LETHE_SYNC_TOKEN` also works in place of a token file. `lethe` doesn't do TLS
itself. Keep the server on a private network such as a tailnet, or put a
reverse proxy in front of it and use an `https://` URL.

### NixOS module

```nix
{
  imports = [ inputs.lethe.nixosModules.default ];

  services.lethe-server = {
    enable = true;
    listenAddress = "0.0.0.0";
    tokenFile = config.age.secrets.lethe-token.path;
  };
  # only reachable over the tailnet
  networking.firewall.interfaces.tailscale0.allowedTCPPorts = [ 7374 ];
}
```

## A little lore

`lethe` is named after [Lethe](https://en.wikipedia.org/wiki/Lethe) one of the
rivers to the underworld from Greek mythology. The word means "forgetting" or
"forgetfulness", which makes the name antithesis. 
