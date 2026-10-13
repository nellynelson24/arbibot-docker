use std::collections::HashMap;

use super::*;

pub(super) fn print_forecast(
    catchable: &[&Episode],
    all: &[&Episode],
    transfer: &Transfer,
    day_scale: f64,
    cyclic_day_scale: f64,
) {
    let mut by_market: HashMap<Key, Vec<&Episode>> = HashMap::new();
    for e in catchable {
        by_market.entry(e.market()).or_default().push(e);
    }
    let mut rows: Vec<(Key, Vec<f64>)> = by_market
        .iter()
        .map(|(&market, list)| {
            let monthly = CAPITALS
                .iter()
                .map(|&c| {
                    let (profit, volume) = simulate(list, market.1, c);
                    repeatable(profit, volume[0], volume[1]) * day_scale * 30.0
                })
                .collect();
            (market, monthly)
        })
        .collect();
    rows.sort_by(|a, b| b.1[1].total_cmp(&a.1[1]).then_with(|| a.0.cmp(&b.0)));

    println!("Prognoza miesięczna (Model 1 — Inwentarzowy: 4 części bez transferów):");
    println!(
        "(zysk po prowizjach taker, przed podatkiem; tylko część powtarzalna: obrót w obie strony)"
    );
    println!(
        "{:<14} {:<20} {:>13} {:>13} {:>13}",
        "Para", "Giełdy", "100 USD", "1 000 USD", "10 000 USD"
    );
    for ((pair, a, b), monthly) in rows.iter().take(TOP_FORECASTS) {
        println!(
            "{:<14} {:<20} {:>9.2} USD {:>9.2} USD {:>9.2} USD",
            pair,
            format!("{a} + {b}"),
            monthly[0],
            monthly[1],
            monthly[2]
        );
    }

    let mut cycle_markets: HashMap<Key, Vec<&Episode>> = HashMap::new();
    for e in all.iter().filter(|e| transfer.allows(e)) {
        cycle_markets.entry(e.market()).or_default().push(e);
    }
    let mut cyclic_pair_rows: Vec<(Key, Vec<f64>)> = cycle_markets
        .iter()
        .map(|(&market, list)| {
            let monthly = CAPITALS
                .iter()
                .map(|&c| {
                    let (start_a, start_b) = (
                        simulate_cyclic(list, market.1, c, transfer).0,
                        simulate_cyclic(list, market.2, c, transfer).0,
                    );
                    start_a.max(start_b) * cyclic_day_scale * 30.0
                })
                .collect();
            (market, monthly)
        })
        .collect();
    cyclic_pair_rows.sort_by(|a, b| b.1[1].total_cmp(&a.1[1]).then_with(|| a.0.cmp(&b.0)));

    println!(
        "\nPrognoza miesięczna (Model 2 — Cykliczny 100% kapitału: Kupno -> Wypłata -> Sprzedaż -> Zwrot):"
    );
    println!(
        "(100% kapitału w jednej transakcji na parze; po sprzedaży na B czeka na ruch powrotny B -> A)"
    );
    println!(
        "{:<14} {:<20} {:>13} {:>13} {:>13}",
        "Para", "Giełdy", "100 USD", "1 000 USD", "10 000 USD"
    );
    for ((pair, a, b), monthly) in cyclic_pair_rows.iter().take(TOP_FORECASTS) {
        println!(
            "{:<14} {:<20} {:>9.2} USD {:>9.2} USD {:>9.2} USD",
            pair,
            format!("{a} + {b}"),
            monthly[0],
            monthly[1],
            monthly[2]
        );
    }

    println!(
        "\nPrognoza miesięczna (Model 3 — Dynamiczny obrót 100% kapitału między dowolnymi giełdami i parami):"
    );
    println!(
        "(100% kapitału płynie tam, gdzie w danej chwili jest okazja: Giełda A -> B -> C -> A...)"
    );
    println!(
        "{:<16} {:>13} {:>13} {:>13} {:>12}",
        "Giełda startowa", "100 USD", "1 000 USD", "10 000 USD", "Transakcje/d"
    );
    for venue in Venue::ALL {
        let name = venue.name();
        let eligible: Vec<&Episode> = all.iter().copied().filter(|e| transfer.allows(e)).collect();
        let (p100, t) = simulate_cyclic(&eligible, name, 100.0, transfer);
        let (p1k, _) = simulate_cyclic(&eligible, name, 1_000.0, transfer);
        let (p10k, _) = simulate_cyclic(&eligible, name, 10_000.0, transfer);
        let daily_trades = (t as f64) * cyclic_day_scale;
        println!(
            "{:<16} {:>9.2} USD {:>9.2} USD {:>9.2} USD {:>10.0}/d",
            name,
            p100 * cyclic_day_scale * 30.0,
            p1k * cyclic_day_scale * 30.0,
            p10k * cyclic_day_scale * 30.0,
            daily_trades
        );
    }

    println!(
        "\nZałożenia i uwagi do modeli:\n\
         - Model 1 (Inwentarz): kapitał podzielony na 4 części (25% każda) na obu giełdach, handel symultaniczny;\n  \
           do miesiąca mnożona tylko część obrotu w obie strony (jednostronny wyczerpuje zapas).\n\
         - Model 2 (Pętla pary): 100% kapitału użyte naraz w parze (A -> B, potem B -> A).\n\
         - Model 3 (Sztafeta): 100% kapitału wędruje przez dowolne pary i giełdy, maksymalizując liczbę obrotów.\n\
         - Wielkość zlecenia ograniczona ofertą (first_size) lub 100% kapitału.\n\
         - Modele 2 i 3: przelew {:.0} s i opłata {:.2} za cykl; okazja musi trwać co najmniej\n  \
           tyle co przelew (sprzedaż dopiero po wpłacie); bez aktywów: {}.\n\
         - Wynik przeskalowany do 30 dni z czasu pracy skanera.",
        transfer.ms / 1000.0,
        transfer.fee,
        transfer.excluded.join(", ")
    );
}

pub(super) fn simulate(episodes: &[&Episode], first_venue: &str, capital: f64) -> (f64, [f64; 2]) {
    let bucket = capital / BUCKETS;

    let mut balances = [(bucket, bucket); 2];
    let side = |venue: &str| usize::from(venue != first_venue);
    let mut profit = 0.0;
    let mut volume = [0.0; 2];
    for e in episodes {
        let (buy, sell) = (side(&e.buy), side(&e.sell));
        let size = e
            .first_size
            .unwrap_or(bucket)
            .min(balances[buy].0)
            .min(balances[sell].1);
        if size <= 0.0 {
            continue;
        }
        balances[buy].0 -= size;
        balances[buy].1 += size;
        balances[sell].1 -= size;
        balances[sell].0 += size;
        volume[buy] += size;
        profit += e.first_net_bps / 1e4 * size;
    }
    (profit, volume)
}

pub(super) fn repeatable(profit: f64, volume_a: f64, volume_b: f64) -> f64 {
    let total = volume_a + volume_b;
    if total <= 0.0 {
        0.0
    } else {
        profit * 2.0 * volume_a.min(volume_b) / total
    }
}

pub(super) fn simulate_cyclic(
    episodes: &[&Episode],
    start_venue: &str,
    capital: f64,
    transfer: &Transfer,
) -> (f64, usize) {
    let mut venue = start_venue;
    let mut available_at = f64::MIN;
    let mut capital = capital;
    let mut profit = 0.0;
    let mut cycles = 0;
    for e in episodes {
        if e.buy != venue || e.start_ms < available_at {
            continue;
        }
        let size = e.first_size.unwrap_or(capital).min(capital);
        let cycle_profit = e.first_net_bps / 1e4 * size - transfer.fee;
        if cycle_profit <= 0.0 {
            continue;
        }
        profit += cycle_profit;
        capital += cycle_profit;
        venue = &e.sell;
        available_at = e.start_ms + transfer.ms;
        cycles += 1;
    }
    (profit, cycles)
}
