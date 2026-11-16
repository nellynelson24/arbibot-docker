# arbibot-docker

Arbitraż spot OKX EEA ↔ Bybit EU w Dockerze. Rust.

## Uruchomienie

```bash
docker compose pull
docker compose up -d                       # skaner, warianty na sucho, raport
docker compose --profile live up -d live   # bot na żywo
docker compose logs -f live
```

Ustawienia bota na żywo są w `compose.yaml`: para `ARBIBOT_LIVE_PAIR` (domyślnie ARB/USDC), transakcja do `ARBIBOT_SIZE` (domyślnie 40 USDC), próg 40 bp, maker na OKX po `ARBIBOT_MAKER_SIZE` (domyślnie 10 USDC) z marżą `ARBIBOT_MAKER_MIN_BPS` (domyślnie 20 bp), dzienny limit straty `ARBIBOT_DAILY_LOSS` (domyślnie 2 USDC).

Pierwszy start na nowej parze: `ARBIBOT_LIVE_FLAGS=--buy-inventory`. Bot kupuje monetę pary za USDC na giełdzie, gdzie stanowi mniej niż połowę wartości, jeśli brakuje co najmniej ćwierci transakcji (10 USDC). Inne monety na kontach (np. STRK) zostają nietknięte. Każdy zakup i salda obu giełd przy starcie idą na Telegram. Transakcja zmniejsza się do dostępnych środków, najmniej do ćwierci `--size`.

## Schemat transakcji

- Gdy Bybit odrzuci zabezpieczenie makera (np. brak środków), bot domyka pozycję na OKX transakcją odwrotną, najwyżej 3 razy na dobę; przy kolejnym odrzuceniu albo nieudanym domknięciu zatrzymuje się. Zlecenia bez jasnego wyniku nie są domykane, tylko zatrzymują bota.
- Środki zarezerwowane dla makera nie są dostępne dla takera ani rebalansu: otwarte zlecenia maker blokują walutę na OKX i środki na zabezpieczenie na Bybit, a wypełnienia czekające na zabezpieczenie blokują je na Bybit.
- `--sequential`: najpierw noga na giełdzie, której cena dłużej stoi (jej oferta zniknie pierwsza), druga dopiero po wypełnieniu, z limitem bez straty względem ceny pierwszej. Gdy pierwsza nie wejdzie, nic się nie dzieje.
- Salda wyrównują się same: kierunek, który je wyrównuje, ma niższy próg (`--restore-min-bps`, 30 bp), gdy przeciwny ma mniej niż połowę jego środków.
- `--trade-rebalance`: gdy kierunek jest zablokowany od 15 min, transakcja odwrotna kosztem do 45 bp, najwyżej raz na 10 min. Wymaga, by kierunek odwrotny miał co najmniej 3 × minimalną transakcję (30 USDC). Nogi rebalansu idą po kolei: najpierw OKX, Bybit dopiero po jej wypełnieniu, z limitem z planu rebalansu. Komunikat o braku środków na Telegramie mówi, czy bot wyrówna salda sam, czy trzeba przelać ręcznie.
- `--skew-bps K` (opcja, A/B na sucho): przechył progów według sald — kierunek wyrównujący salda ma próg niższy o K × nierównowagę, rozjeżdżający wyższy; tak samo marża makera. Przechył nigdy nie przekracza ±K, także gdy zlecenia maker rezerwują więcej, niż jest na koncie. Salda wyrównują się przy okazji zarabiających transakcji.
- `--lazy-rebalance` (opcja, A/B na sucho): płatny rebalans handlem tylko po sygnale straconym z braku środków, najwyżej raz na 30 min i 4 razy na dobę.
- `--rebalance-strict` (opcja, A/B na sucho): reszta rebalansu handlem z limitem z planu zamiast 1%, a próba bez wypełnienia nie zużywa dobowego limitu (ponowienie po 60 s).
- `--maker-rest` (opcja, A/B na sucho): zlecenie maker stoi stale po cenie, przy której marża wobec książki Bybit równa się progowi (marża minus przechył), nie lepiej niż najlepsza cena OKX; przestawiane najwyżej co `--requote-ms`. Bez opcji zlecenie stoi tylko na najlepszej cenie OKX i tylko wtedy, gdy ta cena daje wymaganą marżę.
- `--maker-latency` (opcja, tylko na sucho): zlecenie maker zaczyna się wypełniać dopiero po `--okx-latency-ms` od złożenia, a anulowane albo przestawione może się jeszcze wypełnić po starej cenie przez ten sam czas, jak na żywo, gdzie anulowanie trwa 100–200 ms. Jedna transakcja z rynku wypełnia najpierw zlecenie z lepszą ceną. Bez opcji zlecenie pojawia się i znika natychmiast.
- `--maker-net-secs N` (opcja, tylko na sucho): po wypełnieniu makera bot czeka do N s na wypełnienie po przeciwnej stronie i zabezpiecza na Bybit tylko różnicę; znettowana część to wiersz `maker_hedge` z `"net": true` (wynik z obu cen OKX po prowizjach maker). Bez zabezpieczenia może zostać najwyżej jedno zlecenie maker (`--maker-size`): kolejne wypełnienie po tej samej stronie od razu zabezpiecza całość. Po N s bot zabezpiecza resztę jak dziś.
- `--switch-maker-hours H` (opcja): wybór pary na starcie i przy `--switch` według symulacji makera ze skanera (`data/maker`, maker OKX, zabezpieczenie Bybit) z ostatnich H godzin, przeliczonej na `--maker-size` (suma marż w bp × wielkość zlecenia bota). Pary przechodzą te same filtry płynności i mają tę samą walutę kwotowania co obecna para. Zysk z okna jest przeliczany na 3 dni, więc próg zmiany jest ten sam co dziś: przewaga ponad trzykrotność kosztu zmiany, czyli zwrot kosztu w dobę, w dwóch ocenach z rzędu. Z `--pair` ta para jest zapasowa, gdy żadna nie przejdzie filtrów. Bez opcji wybór według okazji takera z 3 dni.
- `--switch-any-route` (opcja): wybór pary bez wymogu taniej trasy przelewu między giełdami, bo salda wyrównuje handel.
- `--hedge-while-paused` (opcja): ponawianie zabezpieczeń makera także w pauzie i po zatrzymaniu, najwyżej co 10 s. Bez niej bot po zatrzymaniu tylko raz pisze na Telegramie, ile monety zostało bez zabezpieczenia.
- `--hedge-leftovers` (opcja, tylko na żywo): na starcie bot porównuje wypełnienia makera z poprzedniej sesji z zabezpieczeniami oraz historię zleceń OKX z dziennikiem. Brakujące zabezpieczenie zawsze zgłasza na Telegramie, a z tą opcją także je wykonuje. Bez opcji sprawdź, czy nie zabezpieczyłeś tego ręcznie.
- Zlecenia maker: przy zamykaniu bot anuluje zlecenia i jeszcze przez 3 s zabezpiecza wypełnienia, które wtedy przychodzą. Co 30 s sprawdza zlecenia bez kwotowania (np. złożone mimo błędu odpowiedzi) i anuluje te, które wiszą na OKX.
- Taker: druga noga i reszta poniżej minimum giełdy zostają jako „niezamknięte” bez zlecenia i bez zatrzymania bota. Gdy jedna noga wejdzie, a druga skończy się błędem, wykonana noga jest księgowana po cenie rynku do wyniku i limitu straty.
- Odczyt sald jest ponawiany 3 razy co 2 s przed zatrzymaniem bota. Klucze do odczytu z prawem wypłat blokują start tak samo jak klucze handlowe. `/stop` działa tylko od właściciela (sprawdzany nadawca) i jest wykonywany także wtedy, gdy przyszedł w ciągu 5 min przed startem.
- `--auto-transfer` (domyślnie wyłączone): przelewy monety i USDC między giełdami z giełdy, której klucz ma prawo wypłat (Bybit EU go nie daje), tylko na adresy wpłat własnych kont; opis w [VPS.md](VPS.md).

Warianty na sucho (`dry` i `ab-*`) mają te same ustawienia co bot na żywo (maker 20 USDC, leniwy rebalans do 30 bp, przechył 15 bp) i różnią się jednym: `ab-arb-strict` z `--rebalance-strict`, `ab-arb-fees50` z prowizjami obu giełd o połowę niższymi (`--fee-scale 0.5`, tylko na sucho; pokazuje, ile dałyby niższe opłaty, np. poziom VIP albo inna giełda), `ab-arb-maker40` z makerem 40 USDC, `ab-arb-rest` z `--maker-rest`, `ab-arb-lat` i `ab-arb-rest-lat` jak `dry` i `ab-arb-rest` z opóźnieniem zleceń maker (`--maker-latency`, sprawdza, czy brak opóźnienia na sucho tłumaczy wyższą marżę niż na żywo), `ab-arb-net60` z netowaniem przeciwnych wypełnień przez 60 s (`--maker-net-secs 60`), `ab-switch` zmienia parę według symulacji makera ze skanera z ostatnich 12 h, z oceną co godzinę i bez wymogu trasy przelewu (`--switch --review-hours 1 --switch-maker-hours 12 --switch-any-route`; ARB/USDC tylko jako para zapasowa), `ab-ena`, `ab-strk`, `ab-s` i `ab-pepe` to inne pary (`ab-pepe` to pomiar monety o cenie rzędu 0,000004, bez planu zmiany pary). `ab-usdc-pln` handluje USDC/PLN z kwotami w PLN po kursie ok. 4 (kapitał 800, transakcja 160, maker 80, limit straty 8 PLN). Ta para ma trwałą różnicę cen ok. 120 bp (OKX drożej niż Bybit EU), a PLN nie da się przelać między giełdami siecią, więc wariant pokazuje, ile zarobi jeden kierunek, zanim skończą się środki. Raport przelicza wyniki wariantów w innej walucie na USDC po kursie z ostatniego wiersza stanu. Usługa `report` porównuje je codziennie o 8:00 czasu polskiego i wysyła skrót na Telegram.

Raport poleca tylko pary, które przechodzą filtry płynności na obu giełdach: obrót 24 h co najmniej 250 tys. USD, spread najwyżej 30 bp i różnica cen między giełdami najwyżej 100 bp (notowania z chwili raportu). Te same filtry stosuje wybór pary bez `--pair`.

Skaner symuluje makera (zlecenie na OKX z marżą 20 bp, zabezpieczenie na Bybit albo Bitvavo) na wszystkich parach. Prowizje Bitvavo zależą od rynku: na rynkach USDC 0,05% maker i taker, na pozostałych 0,15% / 0,25%. Raport pokazuje osobny ranking dla zabezpieczenia na Bitvavo, żeby sprawdzić, czy ta giełda opłaca się przed napisaniem jej obsługi w bocie, oraz ile par miało wypełnienia w symulacji i dlaczego odpadły (np. „brak notowań na Bitvavo 12, obrót OKX 5”). Raport układa pary po filtrach płynności według szacowanego zysku makera na dobę z 14 dni danych skanera, niezależnie od okna raportu, i pokazuje obok wynik z okna. Zysk na dobę liczy się z godzin, w których skaner zapisywał dane; raport podaje, ile godzin danych jest w oknie, wypisuje dni z mniej niż 23 h danych i ostrzega, gdy w oknie jest mniej niż 80% godzin. Radę o zmianie pary daje tylko wtedy, gdy przewaga z 14 dni zwraca koszt zmiany (~0,4 USDC) w tydzień, a para wygrywa w każdym z dwóch tygodni; para najlepsza tylko w oknie dostaje uwagę w pełnym raporcie, bez rady.

## Dane rynkowe

- Świeżość: ping co 2 s na obu giełdach; dane są świeże, gdy pong doszedł najpóźniej 3 s temu, a książka jest zgodna. Bez świeżości obu giełd nie ma sygnałów takera, a zlecenia maker są wycofywane.
- Zgodność książek: delty Bybit muszą mieć kolejne numery `u` (snapshot albo `u = 1` resetuje książkę, duplikat jest pomijany). Luka albo skrzyżowana książka (kupno ≥ sprzedaż) oznacza książkę jako niezgodną i wymusza nową subskrypcję. Licznik luk jest w statusie i w raporcie dziennym.

## Obraz i wersje

GitHub Actions buduje `ghcr.io/nellynelson24/arbibot-docker` z każdego tagu `vX.Y.Z` (obraz `X.Y.Z`, `X.Y`, `latest`) i z gałęzi `main` (obraz `main`). Nowa wersja:

```bash
git tag v1.5.0 && git push origin v1.5.0
```

Zmienne `compose.yaml`: `ARBIBOT_VERSION` (domyślnie `latest`), `ARBIBOT_DATA` (`./data`), `ARBIBOT_SECRETS` (`~/.arbibot/secrets`). Budowa lokalna: `docker build -t ghcr.io/nellynelson24/arbibot-docker:latest .`

## Klucze

Pliki w `~/.arbibot/secrets/`, opis w [VPS.md](VPS.md). Klucze do odczytu i handlu bez prawa wypłat (z prawem wypłat bot nie wystartuje); wypłaty tylko osobnymi kluczami `federbot-<giełda>-withdraw`. Kontener działa jako UID 1000: `data/` i klucze muszą być dla niego czytelne.

## Zatrzymanie

- `docker compose stop live` kończy pracę i anuluje zlecenia maker.
- `touch data/STOP` albo `/stop` na Telegramie wstrzymuje handel. Plik STOP obecny przy starcie zatrzymuje bota przed pierwszym zleceniem i zakupem inwentarza.
- Po błędzie zlecenia albo limicie straty bot sam tworzy `data/STOP`; komunikat podaje, które nogi zostały wykonane. Wznowienie: usuń plik i `docker compose restart live`.

## Raport

Raport na żywo pokazuje też próby zabezpieczeń makera: udane i chybione, wiek książki Bybit i czas od wypełnienia (p50/p90) oraz poślizg wobec ceny z książki, z wnioskiem, czy chybienia biorą się z opóźnionych danych Bybit, czy z ruchu ceny. `/stan` na Telegramie to lista: uwagi (zatrzymanie, brak środków z bieżącym kosztem rebalansu handlem wobec limitu), kapitał bota (moneta i waluta pary na kontach handlowych) z saldami obu giełd, wyniki od restartu (maker, taker, rebalans, zmiana ceny monety), salda poza botem (inne monety i konta funding, z wyceną giełdy) oraz zysk z arbitrażu z dziennika (dziś, 7 i 30 dni, od początku z rozbiciem na maker, taker i rebalanse, od zmiany pary) i rozliczenie kapitału od początku: na starcie + zmiany sald poza botem + zysk + zmiana cen = teraz. Zmiany sald poza botem to skoki kapitału między restartami bez ruchu ceny w czasie postoju (od 1.12.2 bot zapisuje salda i cenę w każdym wierszu stanu) i różnice od 1 USDC między saldami giełd a księgowaniem bota w trakcie pracy (bot pisze o nich na Telegramie). Przelew zrobiony przy działającym bocie przed wersją 1.12.0 dopisuje się ręcznie wierszem `{"type":"outside_correction","value":-10}` w dzienniku `data/live/<data>.jsonl` (kwota w walucie kwotowania, minus dla wypłaty); bot uwzględni go po restarcie. Wiadomość „Start” podaje ustawienia makera i takera, salda i to samo rozliczenie, a podsumowanie dnia wynik dnia i rozliczenie. Raport dzienny o 8:00 wysyła usługa `report`; `live` wysyła go tylko awaryjnie o 8:10, gdy `report` go nie wysłał. Wynik „Na żywo: razem” w raporcie obejmuje rebalanse. Puste próby takera nie liczą się jako transakcje, a wynik w bp liczy się od wypełnionej wartości. Restart nie zeruje dobowych limitów domknięć i rebalansów. Wiadomości na Telegram są ponawiane przy błędzie sieci i limicie Telegrama i przycinane do 4096 znaków; gdy polecenia odbiera inny proces z tym samym tokenem, bot pisze o tym raz i ponawia co minutę.

Od 1.14.0 wiersz `maker_fill` ma `expected_bps` (marża wobec książki Bybit w chwili wypełnienia) i `threshold_bps` (obowiązujący próg: marża minus przechył), a wiersz `status` ma `maker_secs`: dla każdej strony makera sekundy od startu ze zleceniem w książce (`resting`), bez zlecenia przez za małą marżę (`margin`), przez próg podniesiony przechyłem (`skew`), przez brak środków (`funds`) i z innych powodów (`other`: nieświeże książki, pauza, odstęp przestawień, ilość poniżej minimum). Tabela „Maker: gdzie ginie marża i wypełnienia” pokazuje dla każdego wariantu z makerem średni próg, oczekiwaną marżę, poślizg zabezpieczenia i wynik, liczbę zabezpieczeń wobec wypełnień w symulacji skanera dla tej samej pary i okna oraz udziały czasu. Wniosek rozkłada nominalną marżę na przechył, wypełnienie poniżej progu, poślizg zabezpieczenia i ruch ceny do zabezpieczenia, i mówi, czy mniej wypełnień niż w symulacji bierze się z braku środków, z progu z przechyłem, czy z miejsca w kolejce. Ceny monet poniżej 0,1 w wiadomościach mają 6 cyfr znaczących.

Od 1.15.0 wiersz `status` ma `pair`, a wiersz `start` ustawienia zmiany pary, opóźnienia i netowania makera. Sekcja „Zmiana pary” w raporcie pokazuje dla wariantów z `--switch` liczbę zmian pary, ich koszt i godziny na każdej parze (z wierszy stanu; przerwy dłuższe niż 5 min się nie liczą). Wynik „razem” wariantu obejmuje koszt zmian pary.

```bash
docker compose exec live bot report
```

## Struktura

- `bins/bot` — bot
- `bins/scanner` — skaner okazji
- `bins/latency`, `bins/pair-scan` — narzędzia pomocnicze
- `crates/account` — API kont
- `crates/venues` — giełdy i prowizje
