## Usage

### Developement

1. Start nix
```bash
nix develop --experimental-features 'nix-command flakes'
```

2. Run script

```bash
cargo run -- --work-dir=[directory] --src-dir=[directory] --backup-dir=[directories]
```

