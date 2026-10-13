# arbibot-docker

Arbitraż spot OKX EEA ↔ Bybit EU w Dockerze. Rust.

## Budowa

```bash
cargo build --release
```

## Użycie

```bash
./target/release/scanner run
./target/release/bot dry-run --pair STRK/USDC --capital 200 --size 20
./target/release/bot live --pair STRK/USDC --size 20 --min-bps 35 --telegram
./target/release/bot report
```

Klucze API w pęku kluczy macOS (`federbot-<giełda>-read`, `federbot-<giełda>-trade`), bez prawa wypłat. Serwer: [VPS.md](VPS.md).

## Docker

```bash
docker compose up -d                       # skaner i bot na sucho
docker compose --profile live up -d live   # na żywo, bez pytania o TAK
docker compose logs -f live
touch data/STOP                            # zatrzymanie handlu
```

Sekret to plik `~/.arbibot/secrets/<usługa>` (`federbot-okx-read`, `federbot-okx-trade`, `federbot-bybit-read`, `federbot-bybit-trade`, `federbot-telegram`) z liniami `api_key=…`, `secret=…`, `passphrase=…` (OKX) albo `token=…`, `chat_id=…`, `viewers=…` (Telegram). Kontener działa jako UID 1000: na Linuksie `data/` i sekrety muszą być dla niego czytelne.

Po błędzie zlecenia albo limicie straty bot na żywo zostawia `data/STOP`; wznowienie: usuń plik i `docker compose restart live`.

## Struktura

- `bins/bot` — bot
- `bins/scanner` — skaner okazji
- `bins/latency` — pomiar opóźnień
- `bins/pair-scan` — przegląd par
- `crates/account` — API kont
- `crates/venues` — giełdy i prowizje
