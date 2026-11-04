use crate::error::Result;
use crate::model::Cents;
use crate::service::Reservations;

const VENUES: [(&str, &str); 8] = [
    ("Harbour House", "Lisbon"),
    ("Alpine Lodge", "Zermatt"),
    ("Canal Inn", "Amsterdam"),
    ("Desert Rose", "Marrakesh"),
    ("Fjord Cabins", "Bergen"),
    ("Old Town Suites", "Prague"),
    ("Lagoon Hotel", "Venice"),
    ("Pine Retreat", "Banff"),
];

#[derive(Clone, Debug)]
pub struct Plan {
    pub venues: u32,
    pub rooms: u32,
    pub customers: u32,
    pub credit: Cents,
    pub rate: Cents,
}

impl Default for Plan {
    fn default() -> Self {
        Plan {
            venues: 3,
            rooms: 4,
            customers: 5,
            credit: 200_000,
            rate: 12_000,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Seeded {
    pub venues: Vec<u32>,
    pub rooms: Vec<u32>,
    pub customers: Vec<u32>,
}

pub fn venue_name(index: u32) -> (String, String) {
    match VENUES.get(index as usize) {
        Some((name, city)) => (name.to_string(), city.to_string()),
        None => (format!("Venue {}", index + 1), "Elsewhere".to_string()),
    }
}

pub fn seed(svc: &Reservations, plan: &Plan) -> Result<Seeded> {
    svc.write(|h, _| {
        let mut seeded = Seeded::default();
        let offset = h.next_venue;
        for v in 0..plan.venues {
            let (name, city) = venue_name(v + offset);
            let venue = h.add_venue(&name, &city)?;
            seeded.venues.push(venue);
            for r in 0..plan.rooms {
                let number = 101 + r as u16;
                let rate = plan.rate + Cents::from(r % 3) * 2_500 + Cents::from(v) * 1_000;
                seeded.rooms.push(h.add_room(venue, number, rate)?);
            }
        }
        for c in 0..plan.customers {
            let n = h.next_customer + 1;
            let customer =
                h.add_customer(&format!("guest{n}@example.com"), &format!("Guest {n}"))?;
            if plan.credit > 0 {
                h.deposit(customer, plan.credit + Cents::from(c % 4) * 25_000)?;
            }
            seeded.customers.push(customer);
        }
        let message = format!(
            "seed {} venues, {} rooms, {} customers",
            seeded.venues.len(),
            seeded.rooms.len(),
            seeded.customers.len()
        );
        Ok((message, seeded))
    })
}
