# Wdrożenie na VPS

## Klucze

Na OKX EEA i Bybit EU utwórz klucze tylko dla serwera: do odczytu i do handlu spot, bez prawa wypłat, z ograniczeniem do adresu IP serwera.

Każdy komplet to plik w katalogu kluczy na serwerze (`/opt/arbibot/secrets`):

| Plik | Linie |
|---|---|
| `federbot-okx-read`, `federbot-okx-trade` | `api_key=…`, `secret=…`, `passphrase=…` |
| `federbot-bybit-read`, `federbot-bybit-trade` | `api_key=…`, `secret=…` |
| `federbot-telegram` | `token=…`, `chat_id=…`, `viewers=…` (może być puste) |

Klucze przekazuj zaszyfrowane (np. `gpg --symmetric`), hasło osobno.

## Serwer

1. Katalogi na dane i klucze, czytelne dla UID 1000 (użytkownik w kontenerze):
   ```bash
   sudo mkdir -p /opt/arbibot/data /opt/arbibot/secrets
   ```
   Wgraj pliki z kluczami do `/opt/arbibot/secrets`, potem:
   ```bash
   sudo chown -R 1000:1000 /opt/arbibot
   sudo chmod 700 /opt/arbibot/secrets && sudo chmod 600 /opt/arbibot/secrets/*
   ```
2. Portainer: Stacks → Add stack → Repository, URL `https://github.com/nellynelson24/arbibot-docker`, plik `compose.yaml`. Zmienne środowiskowe:

   | Zmienna | Wartość |
   |---|---|
   | `ARBIBOT_VERSION` | np. `1.0.0` |
   | `ARBIBOT_DATA` | `/opt/arbibot/data` |
   | `ARBIBOT_SECRETS` | `/opt/arbibot/secrets` |
   | `COMPOSE_PROFILES` | `live`, gdy ma działać bot na żywo |
   | `ARBIBOT_LIVE_FLAGS` | `--buy-inventory` tylko przy pierwszym starcie na nowej parze, potem puste |

   Bez Portainera: te same zmienne w pliku `.env` obok `compose.yaml`, potem `docker compose pull && docker compose up -d`.
3. Aktualizacja: nowa wartość `ARBIBOT_VERSION` i redeploy z pobraniem obrazu.

## Bot na żywo

Na żywo może działać tylko jeden bot naraz: najpierw zatrzymaj tego na Macu.

1. Mac: `docker compose stop live`.
2. Prześlij dziennik, potrzebny do dziennego limitu straty:
   ```bash
   rsync -a data/live/ UŻYTKOWNIK@SERWER:/opt/arbibot/data/live/
   ```
3. Serwer: `COMPOSE_PROFILES=live` w Portainerze (albo `docker compose --profile live up -d live`).

Zatrzymanie handlu: `touch /opt/arbibot/data/STOP` albo `/stop` na Telegramie.
