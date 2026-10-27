# Wdrożenie na VPS

## Klucze

Na OKX EEA i Bybit EU utwórz klucze tylko dla serwera: do odczytu i do handlu spot, bez prawa wypłat, z ograniczeniem do adresu IP serwera.

Na OKX w kluczu handlowym zaznacz na liście Crypto monetę pary (np. TIA) albo wszystkie waluty. Bez tego każde zlecenie kończy się kodem 50123 „This API Key does not have trading permission for the Crypto”.

Każdy komplet to plik w katalogu kluczy na serwerze (`/opt/arbibot/secrets`):

| Plik | Linie |
|---|---|
| `federbot-okx-read`, `federbot-okx-trade` | `api_key=…`, `secret=…`, `passphrase=…` |
| `federbot-bybit-read`, `federbot-bybit-trade` | `api_key=…`, `secret=…` |
| `federbot-telegram` | `token=…`, `chat_id=…`, `viewers=…` (może być puste) |

Klucze przekazuj zaszyfrowane (np. `gpg --symmetric`), hasło osobno.

### Przelewy między giełdami (opcjonalne, domyślnie wyłączone)

Salda wyrównuje handel. Automatyczne przelewy włącza `ARBIBOT_LIVE_FLAGS=--auto-transfer` razem z kluczami do wypłat (pliki trzeba dodać też do `secrets` usługi `live` w `compose.yaml`).


- Klucze do wypłat, osobne od handlowych, z ograniczeniem do IP serwera: OKX z prawem odczytu i wypłat; Bybit z „Transfer na konto” (Bybit EU nie daje prawa wypłat przez API, więc z Bybit salda wyrównuje handel albo przelew ręczny, a OKX przelewa na Bybit sam).
- W książkach adresowych wpisz adresy wpłat drugiej giełdy: na OKX adresy z Bybit, na Bybit adresy z OKX, dla monety pary (TIA, sieć Celestia) i USDC (sieć Arbitrum). Giełdy wypłacają tylko na adresy z książki.
- Bot przelewa połowę różnicy, gdy na giełdzie zostaje mniej niż 40% poziomu docelowego: najwyżej 6 przelewów na dobę i 100 USDC na przelew, tylko na adresy wpłat własnych kont. Środki na kontach funding (także ręczne wpłaty) przenosi na trading.
- Test w konsoli kontenera `arbibot-live-1`: `bot transfer --asset USDC --from bybit --plan` pokazuje trasę i adres bez wysyłania; `bot transfer --asset USDC --from bybit --amount 2` wysyła 2 USDC po potwierdzeniu TAK.

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
   | `ARBIBOT_VERSION` | np. `1.5.0` |
   | `ARBIBOT_DATA` | `/opt/arbibot/data` |
   | `ARBIBOT_SECRETS` | `/opt/arbibot/secrets` |
   | `COMPOSE_PROFILES` | `live`, gdy ma działać bot na żywo |
   | `ARBIBOT_LIVE_PAIR` | para bota na żywo, domyślnie `ARB/USDC` |
   | `ARBIBOT_MAKER_MIN_BPS` | marża makera na żywo w bp, domyślnie `20` |
   | `ARBIBOT_SIZE` | transakcja takera na żywo (USDC), domyślnie `40`, najwyżej `50` |
   | `ARBIBOT_MAKER_SIZE` | zlecenie maker na żywo (USDC), domyślnie `10`, najwyżej `50` |
   | `ARBIBOT_DAILY_LOSS` | dzienny limit straty na żywo (USDC), domyślnie `2` |
   | `ARBIBOT_LIVE_FLAGS` | `--buy-inventory` tylko przy pierwszym starcie na nowej parze (kupuje brakującą monetę pary, innych nie sprzedaje), potem puste |

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

Zmiana pary na żywo:
1. Na OKX dodaj nową monetę do listy Crypto w kluczu handlowym.
2. Zatrzymaj `live` i sprzedaj starą monetę za USDC na obu giełdach.
3. `ARBIBOT_LIVE_PAIR` na nową parę, `ARBIBOT_LIVE_FLAGS` z `--buy-inventory`, redeploy.
4. Po wiadomości „Start” z saldami nowej monety usuń `--buy-inventory` z `ARBIBOT_LIVE_FLAGS`.
