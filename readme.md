# PvR SimulationCraft - RaidBots rip-off project

This aplication is writen in **Rust**

# Functions

* **QuickSim** Sims your currently equipped gear and talents, requires simulationcraft addon output.
* **TopGear** Allows you to select gear from you bag and use it to find the best combination for you.
* **Accounts** Allows you to create a account to store your simulations, password is hashed and not stored in plain text ;)
* **Roles** Add-on to accounts (User, Premium, Admin)
* **Queue** Every simulation request is added to a queue to help descrease the load on server
* **Premium** Has it's own priority queue to skip ahead normal Users
* **History** Shows you your simulation history in you Account tab

# Technologies

* **Rust**
* **Axum**
* **SQLite**
* **Tokio**
* **Argon2**
* And more...

# Requirements
1. **Rust & Cargo** [Install Rust](https://www.rust-lang.org/tools/install)
2. **Blizzard API** Create a .env file with BLIZZARD_CLIENT_ID, BLIZZARD_CLIENT_SECRET
3. **SimulationCraft CLI** [SimC](https://www.simulationcraft.org/download.html), extract the folder to be on same level as project folder!