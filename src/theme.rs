// TODO later...
// If configured to, run scripts in XDG_DATA_DIR/dark-mode.d/ or XDG_DATA_DIR/light-mode.d/
// when the theme is set to auto-export color palette, write to gtk3 / gtk4 / kde / ... css files
// read config file for lat/long

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::bail;
use chrono::{DateTime, Datelike, Days, Local};
use cosmic_config::CosmicConfigEntry;
use cosmic_theme::ThemeMode;
use geoclue2::LocationProxy;
use tokio::time::Instant;
use tokio_stream::StreamExt;

use crate::DBUS_NAME;

#[derive(Debug)]
pub struct SunriseSunset {
    last_update: DateTime<Local>,
    sunrise: Instant,
    sunset: Instant,
    lat: f64,
    long: f64,
}

impl SunriseSunset {
    pub fn new(lat: f64, long: f64, now: DateTime<Local>) -> anyhow::Result<Self> {
        let system_now = SystemTime::from(now);
        let instant_now = Instant::now();
        let year = now.year();
        let month = now.month();
        let day = now.day();
        let (sunrise, sunset) = sunrise::sunrise_sunset(lat, long, year, month, day);

        let Some(sunrise) =
            UNIX_EPOCH.checked_add(std::time::Duration::from_secs(u64::try_from(sunrise)?))
        else {
            bail!("Failed to calculate sunrise time");
        };
        let Some(sunset) =
            UNIX_EPOCH.checked_add(std::time::Duration::from_secs(u64::try_from(sunset)?))
        else {
            bail!("Failed to calculate sunset time");
        };

        let st_to_instant = |now: SystemTime, st: SystemTime| -> anyhow::Result<Instant> {
            Ok(if st > now {
                instant_now
                    .checked_add(st.duration_since(now)?)
                    .ok_or(anyhow::anyhow!("Failed to convert system time to instant"))?
            } else {
                instant_now
                    .checked_sub(now.duration_since(st)?)
                    .ok_or(anyhow::anyhow!("Failed to convert system time to instant"))?
            })
        };

        Ok(Self {
            last_update: now,
            sunrise: st_to_instant(system_now, sunrise)?,
            sunset: st_to_instant(system_now, sunset)?,
            lat,
            long,
        })
    }

    pub fn is_dark(&self) -> anyhow::Result<bool> {
        if self.last_update.date_naive() != Local::now().date_naive() {
            bail!("SunriseSunset out of date");
        }

        let now = Instant::now();
        Ok(now < self.sunrise || now >= self.sunset)
    }

    pub fn next(&self) -> anyhow::Result<Instant> {
        let now = Instant::now();
        if self.sunrise.checked_duration_since(now).is_some() {
            Ok(self.sunrise)
        } else if self.sunset.checked_duration_since(now).is_some() {
            Ok(self.sunset)
        } else {
            bail!("SunriseSunset instants have already passed...");
        }
    }

    pub fn update_next(&mut self) -> anyhow::Result<Instant> {
        match self.next() {
            Ok(i) => Ok(i),
            Err(_) => {
                let Some(tomorrow) = self.last_update.checked_add_days(Days::new(1)) else {
                    bail!("Failed to calculate next date for theme auto-switch.");
                };
                *self = Self::new(self.lat, self.long, tomorrow)?;
                self.next()
            }
        }
    }
}

pub async fn watch_theme(
    theme_mode_rx: &mut tokio::sync::mpsc::Receiver<String>,
) -> anyhow::Result<()> {
    let helper = ThemeMode::config()?;
    let mut theme_mode = match ThemeMode::get_entry(&helper) {
        Ok(t) => t,
        Err((errs, t)) => {
            for why in errs {
                eprintln!("{why}");
            }
            t
        }
    };

    let conn = zbus::Connection::system().await?;
    let mgr = geoclue2::ManagerProxy::new(&conn).await?;
    let client = mgr.get_client().await?;
    client.set_desktop_id(DBUS_NAME).await?;
    // TODO allow preference for config file instead?
    let mut location_updates = Some(client.receive_location_updated().await?);
    client.start().await?;

    let mut sunrise_sunset: Option<SunriseSunset> = None;
    loop {
        let sunset_deadline = if let Some(s) = sunrise_sunset.as_mut() {
            Some(s.update_next()?)
        } else {
            None
        };

        let location_update = async {
            if let Some(location_updates) = location_updates.as_mut() {
                location_updates.next().await
            } else {
                std::future::pending().await
            }
        };

        let sleep = async move {
            if !theme_mode.auto_switch {
                std::future::pending().await
            } else if let Some(s) = sunset_deadline {
                tokio::time::sleep_until(s).await
            } else {
                std::future::pending().await
            }
        };

        tokio::select! {
            changes = theme_mode_rx.recv() => {
                let Some(changes) = changes else {
                    bail!("Theme mode changes failed");
                };

                let auto_switch_prev = theme_mode.auto_switch;
                let (errs, _) = theme_mode.update_keys(&helper, &[changes]);

                for err in errs {
                    eprintln!("Error updating the theme mode {err:?}");
                }

                // need to set the theme right away
                if !theme_mode.auto_switch && auto_switch_prev {
                    let Some(is_dark) = sunrise_sunset.as_ref().and_then(|s| s.is_dark().ok()) else {
                        continue;
                    };

                    if let Err(err) = theme_mode.set_is_dark(&helper, is_dark) {
                        eprintln!("Failed to update theme mode {err:?}");
                    }
                }
            }
            _ = sleep => {
                if !theme_mode.auto_switch {
                    continue;
                }
                // update the theme mode
                let Some(is_dark) = sunrise_sunset.as_ref().and_then(|s| s.is_dark().ok()) else {
                    continue;
                };

                if let Err(err) = theme_mode.set_is_dark(&helper, is_dark) {
                    eprintln!("Failed to update theme mode {err:?}");
                }
            }
            location_update = location_update => {
                // set the next timer
                // update the theme if necessary
                let Some(location_update) = location_update else {
                    bail!("No location in the update");
                };
                let args = location_update.args()?;
                let new = LocationProxy::builder(&conn)
                    .path(args.new())?
                    .build()
                    .await?;
                let latitude = new.latitude().await?;
                let longitude = new.longitude().await?;

                match SunriseSunset::new(latitude, longitude, Local::now()) {
                    Ok(s) => {
                        sunrise_sunset = Some(s);
                    },
                    Err(err) => {
                        eprintln!("Failed to calculate sunrise and sunset for current location {err:?}");
                    },
                };

                let Some(is_dark) =  sunrise_sunset.as_ref().unwrap().is_dark().ok() else {
                    continue;
                };

                if let Err(err) = theme_mode.set_is_dark(&helper, is_dark) {
                    eprintln!("Failed to update theme mode {err:?}");
                }
            }

        }
    }
}