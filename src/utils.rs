use dashmap::DashMap;
use regex::Regex;
use serde::Serialize;
use serde_json::Value;
use sqlx::{Pool, Sqlite};
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
    time::{Instant, SystemTime},
};
use tokio::sync::{Mutex, mpsc};
use tower_sessions::Session;

#[derive(Clone)]
pub struct BlizzardAuth {
    pub client_id: String,
    pub client_secret: String,
    pub access_token: Option<String>,
    pub expires_at: Instant,
}

#[derive(Clone)]
pub struct AppState {
    pub premium_queue: mpsc::Sender<SimulationJob>,
    pub standard_queue: mpsc::Sender<SimulationJob>,
    pub queue_tracker: Arc<Mutex<Vec<QueueItem>>>,
    pub job_statuses: Arc<DashMap<String, SimStatus>>,
    pub http_client: reqwest::Client,
    pub blizzard_auth: Arc<Mutex<BlizzardAuth>>,
    pub item_icon_cache: Arc<DashMap<String, String>>, // OPTIMALIZATION to not reach Blizzard API limit so fast (100x/60secs)
    pub db: Pool<Sqlite>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub enum SimStatus {
    Queued,
    Processing,
    TopGearProcessing { current: usize, total: usize },
    Finished,
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct QueueItem {
    pub id: String,
    pub is_premium: bool,
    pub created_at: SystemTime,
}

#[derive(Debug)]
pub enum JobType {
    QuickSim {
        base_simc: String,
    },
    TopGear {
        base_simc: String,
        selected_items: HashMap<String, Vec<String>>,
    },
}

pub struct SimulationJob {
    pub id: String,
    pub job_type: JobType,
    pub user_id: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct GearItem {
    pub name: String,
    pub id: String,
    pub ilvl: String,
    pub slot: String,
    pub bonus_ids: String,
    pub gem_ids: Vec<String>,
    pub is_equipped: bool,
    pub raw_line: String,
}

pub async fn generate_header(session: &Session) -> String {
    let username: Option<String> = session.get("username").await.unwrap_or(None);

    if let Some(user) = username {
        format!(
            r#"
            <div class="user-menu">
                <button class="dropbtn" style="background-color: #ffcc00; color: black;">{} ▼</button>
                <div class="dropdown-content">
                    <a href="/user/profile/{}" style="color:#ffcc00; border-bottom:1px solid #444;">My Profile</a>
                    <a href="/user/logout" style="color:#ff5555;">Logout</a>
                </div>
            </div>
            "#,
            user, user
        )
    } else {
        r#"
        <div class="user-menu">
            <button class="dropbtn">Login ▼</button>
            <div class="dropdown-content" style="padding: 10px;">
                <form action="/user/login" method="post">
                    <input type="text" name="username" placeholder="Username" required style="width: 90%; margin-bottom: 5px; padding: 5px; background: #111; border: 1px solid #555; color: white;">
                    <input type="password" name="password" placeholder="Password" required style="width: 90%; margin-bottom: 5px; padding: 5px; background: #111; border: 1px solid #555; color: white;">
                    <button type="submit" style="width: 100%; cursor: pointer; background: #4CAF50; border: none; color: white; padding: 5px;">Login</button>
                </form>
                <div style="margin-top:5px; text-align:center;">
                    <a href="/user/register" style="font-size: 0.8em; padding: 5px;">Register</a>
                </div>
            </div>
        </div>
        "#.to_string()
    }
}

pub fn parse_simc_input(input: &str) -> BTreeMap<String, Vec<GearItem>> {
    let mut items_by_slot: BTreeMap<String, Vec<GearItem>> = BTreeMap::new();

    // Used to catch equipped item name
    // # Augur's Ephemeral Wide-Brim (704)
    let re_name_ilvl = Regex::new(r"^#\s+(.*?)\s+\((\d+)\)\s*$").unwrap();

    // Used to catch equipped item stats
    // head=,id=237718,gem_id=...
    let re_equipped = Regex::new(r"^([a-z0-9_]+)=,id=(\d+)(.*)$").unwrap();

    // Used to catch bagged item stats
    // # head=,id=223294,bonus_id=...
    let re_bag = Regex::new(r"^#\s+([a-z0-9_]+)=,id=(\d+)(.*)$").unwrap();

    let lines: Vec<&str> = input.lines().collect();

    for i in 1..lines.len() {
        let current_line = lines[i].trim();
        let prev_line = lines[i - 1].trim();

        let mut item: Option<GearItem> = None;

        let parse_params = |rest: &str| -> (String, Vec<String>) {
            let mut bonus = String::new();
            let mut gems = Vec::new();

            for part in rest.split(',') {
                let part = part.trim();
                if part.starts_with("bonus_id=") {
                    bonus = part.replace("bonus_id=", "");
                } else if part.starts_with("gem_id=") {
                    let gem_str = part.replace("gem_id=", "");
                    gems = gem_str.split('/').map(|s| s.to_string()).collect();
                }
            }
            (bonus, gems)
        };

        if let Some(caps) = re_equipped.captures(current_line) {
            if let Some(name_caps) = re_name_ilvl.captures(prev_line) {
                let (bonus, gems) = parse_params(&caps[3]);
                item = Some(GearItem {
                    name: name_caps[1].to_string(),
                    ilvl: name_caps[2].to_string(),
                    slot: caps[1].to_string(),
                    id: caps[2].to_string(),
                    bonus_ids: bonus,
                    gem_ids: gems,
                    is_equipped: true,
                    raw_line: current_line.to_string(),
                });
            }
        } else if let Some(caps) = re_bag.captures(current_line) {
            if let Some(name_caps) = re_name_ilvl.captures(prev_line) {
                let (bonus, gems) = parse_params(&caps[3]);
                let clean_line = format!("{}=,id={}{}", &caps[1], &caps[2], &caps[3]);
                item = Some(GearItem {
                    name: name_caps[1].to_string(),
                    ilvl: name_caps[2].to_string(),
                    slot: caps[1].to_string(),
                    id: caps[2].to_string(),
                    bonus_ids: bonus,
                    gem_ids: gems,
                    is_equipped: false,
                    raw_line: clean_line,
                });
            }
        }

        if let Some(final_item) = item {
            let display_slot = match final_item.slot.as_str() {
                "finger1" | "finger2" => "Finger".to_string(),
                "trinket1" | "trinket2" => "Trinket".to_string(),
                "main_hand" => "Main Hand".to_string(),
                "off_hand" => "Off Hand".to_string(),
                s => {
                    let mut c = s.chars();
                    match c.next() {
                        None => String::new(),
                        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                    }
                }
            };
            items_by_slot
                .entry(display_slot)
                .or_insert_with(Vec::new)
                .push(final_item);
        }
    }
    items_by_slot
}

pub fn extract_dps_from_json(json_str: &str, sim_type: &str) -> Option<f64> {
    let v: Value = serde_json::from_str(json_str).ok()?;
    if sim_type == "TopGear" {
        return v.get("results")?.get(0)?.get("dps")?.as_f64();
    } else {
        return v
            .get("sim")?
            .get("players")?
            .get(0)?
            .get("collected_data")?
            .get("dps")?
            .get("mean")?
            .as_f64();
    }
}

/* TESTS */
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_dps_quicksim() {
        let json_data = r#"
        {
            "sim": {
                "players": [
                    {
                        "collected_data": {
                            "dps": { "mean": 12345.67 }
                        }
                    }
                ]
            }
        }
        "#;
        let dps = extract_dps_from_json(json_data, "QuickSim");
        assert_eq!(dps, Some(12345.67));
    }

    #[test]
    fn test_extract_dps_topgear() {
        let json_data = r#"
        {
            "results": [
                { "dps": 9999.9, "items": [] }
            ]
        }
        "#;
        let dps = extract_dps_from_json(json_data, "TopGear");
        assert_eq!(dps, Some(9999.9));
    }

    #[test]
    fn test_extract_dps_invalid() {
        let dps = extract_dps_from_json("invalid json", "QuickSim");
        assert_eq!(dps, None);
    }

    #[test]
    fn test_parse_simc_equipped_item() {
        // Simulate a simc input of a equipped item
        let input = r#"
            # Mythic Helmet (730)
            head=,id=12345,bonus_id=666
        "#;
        let map = parse_simc_input(input);

        assert!(map.contains_key("Head"));
        let items = &map["Head"];
        assert_eq!(items.len(), 1);

        let item = &items[0];
        assert_eq!(item.id, "12345");
        assert_eq!(item.bonus_ids, "666");
        assert_eq!(item.name, "Mythic Helmet");
        assert!(item.is_equipped);
    }

    #[test]
    fn test_parse_simc_bag_item() {
        // Simulate a item in bag and '#' cleaning
        let input = r#"
            # Mythic Helmet (730)
            # head=,id=12345,bonus_id=666/67,gem_id=69
        "#;
        let map = parse_simc_input(input);

        assert!(map.contains_key("Head"));
        let item = &map["Head"][0];

        assert_eq!(item.id, "12345");
        assert_eq!(item.gem_ids.len(), 1);
        assert_eq!(item.gem_ids[0], "69");
        assert!(!item.is_equipped);

        assert_eq!(item.raw_line, "head=,id=12345,bonus_id=666/67,gem_id=69");
    }

    #[test]
    fn test_parse_simc_slots_mapping() {
        // Simulate and test ring mapping
        let input = r#"
            # Ring 1 (600)
            finger1=,id=1
            # Ring 2 (600)
            finger2=,id=2
        "#;
        let map = parse_simc_input(input);
        assert!(map.contains_key("Finger"));
        assert_eq!(map["Finger"].len(), 2);
    }
}
