# Wdrożenie na VPS

## Klucze

Na OKX EEA i Bybit EU utwórz klucze tylko dla serwera: do odczytu i do handlu spot, bez prawa wypłat, z ograniczeniem do adresu IP serwera.

Każdy komplet to plik w `~/.arbibot/secrets/` na serwerze:

| Plik | Linie |
|---|---|
| `federbot-okx-read`, `federbot-okx-trade` | `api_key=…`, `secret=…`, `passphrase=…` |
| `federbot-bybit-read`, `federbot-bybit-trade` | `api_key=…`, `secret=…` |
| `federbot-telegram` | `token=…`, `chat_id=…`, `viewers=…` (może być puste) |

Klucze przekazuj zaszyfrowane (np. `gpg --symmetric`), hasło osobno.

## Serwer

1. Zainstaluj Docker Engine z wtyczką compose: `systemctl enable --now docker`.
2. Pobierz kod:
   ```bash
   git clone https://github.com/nellynelson24/arbibot-docker.git
   cd arbibot-docker && mkdir -p data
   ```
3. Uprawnienia. Kontener działa jako UID 1000; przy innym użytkowniku serwera (np. root) także `chown`:
   ```bash
   chmod 700 ~/.arbibot/secrets && chmod 600 ~/.arbibot/secrets/*
   sudo chown -R 1000:1000 ~/.arbibot/secrets data
   ```
4. Skaner i bot na sucho:
   ```bash
   docker compose build && docker compose up -d
   ```

## Bot na żywo

Na żywo może działać tylko jeden bot naraz: najpierw zatrzymaj tego na Macu.

1. Mac: `docker compose stop live`.
2. Prześlij dziennik, potrzebny do dziennego limitu straty:
   ```bash
   rsync -a data/live/ UŻYTKOWNIK@SERWER:arbibot-docker/data/live/
   ```
3. Serwer: `docker compose --profile live up -d live`, potem `docker compose logs -f live`.

Zatrzymanie handlu: `touch data/STOP` albo `/stop` na Telegramie.
