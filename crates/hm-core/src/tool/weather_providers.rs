//! Provider-specific URL builders, response parsers, fixed condition tables,
//! and built-in Chinese city coordinates/codes for the weather service.
//!
//! Security & honesty invariants:
//! - Provider prose is NEVER copied into `condition` or `rendered_answer`;
//!   every provider maps its numeric/symbolic weather code through a fixed
//!   compile-time table (`condition_word`, `met_no_condition`, `wttr_condition`,
//!   `cn_condition`).

use super::{Day, Reading, WeatherError};
use crate::config::WeatherPlace;
use serde_json::{json, Value};

pub(super) const USER_AGENT: &str = "HomeMate/0.1 (+https://github.com/yes999zc/home-mate-b)";
pub(super) const DEFAULT_FORECAST: &str = "https://api.open-meteo.com/v1/forecast";
pub(super) const DEFAULT_MET_NO: &str =
    "https://api.met.no/weatherapi/locationforecast/2.0/compact";
pub(super) const DEFAULT_WTTR_IN: &str = "https://wttr.in";
pub(super) const DEFAULT_GEOCODE: &str = "https://geocoding-api.open-meteo.com/v1/search";

pub(super) fn provider_url(
    provider: &str,
    endpoint: &str,
    place: &WeatherPlace,
    days: u8,
    key: Option<&str>,
) -> Result<String, WeatherError> {
    match provider {
        "" | "open_meteo" | "open-meteo" => Ok(open_meteo_url(endpoint, place, days, key)),
        "met_no" | "met-no" | "met_norway" | "met-norway" => Ok(met_no_url(endpoint, place, key)),
        "wttr_in" | "wttr-in" | "wttr" => Ok(wttr_in_url(endpoint, place, key)),
        _ => Err(WeatherError::Provider),
    }
}

pub(super) fn parse_provider_reading(
    provider: &str,
    body: &Value,
    days: u8,
) -> Result<Reading, WeatherError> {
    match provider {
        "" | "open_meteo" | "open-meteo" => parse_reading(body, days),
        "met_no" | "met-no" | "met_norway" | "met-norway" => parse_met_no(body, days),
        "wttr_in" | "wttr-in" | "wttr" => parse_wttr_in(body, days),
        _ => Err(WeatherError::Provider),
    }
}

fn open_meteo_url(endpoint: &str, place: &WeatherPlace, days: u8, key: Option<&str>) -> String {
    let base = if endpoint.trim().is_empty() {
        DEFAULT_FORECAST.to_string()
    } else {
        let b = endpoint.trim().trim_end_matches('/');
        if b.ends_with("/v1/forecast") {
            b.to_string()
        } else {
            format!("{b}/v1/forecast")
        }
    };
    let tz = if place.timezone.is_empty() {
        "auto"
    } else {
        place.timezone.as_str()
    };
    let mut url = format!(
        "{base}?latitude={:.5}&longitude={:.5}\
         &current=temperature_2m,apparent_temperature,relative_humidity_2m,precipitation,weather_code,wind_speed_10m\
         &daily=weather_code,temperature_2m_max,temperature_2m_min,precipitation_probability_max\
         &timezone={}&forecast_days={}&wind_speed_unit=kmh",
        place.latitude,
        place.longitude,
        urlencoding(tz),
        days.clamp(1, 7)
    );
    if let Some(k) = key.filter(|k| !k.is_empty()) {
        url.push_str(&format!("&apikey={}", urlencoding(k)));
    }
    url
}

fn met_no_url(endpoint: &str, place: &WeatherPlace, key: Option<&str>) -> String {
    let base = if endpoint.trim().is_empty() {
        DEFAULT_MET_NO.to_string()
    } else {
        let b = endpoint.trim().trim_end_matches('/');
        if b.contains("/weatherapi/") || b.ends_with("/compact") || b.ends_with("/complete") {
            b.to_string()
        } else {
            format!("{b}/weatherapi/locationforecast/2.0/compact")
        }
    };
    let mut url = format!(
        "{base}?lat={:.4}&lon={:.4}",
        place.latitude, place.longitude
    );
    if let Some(k) = key.filter(|k| !k.is_empty()) {
        url.push_str(&format!("&apikey={}", urlencoding(k)));
    }
    url
}

fn wttr_in_url(endpoint: &str, place: &WeatherPlace, key: Option<&str>) -> String {
    let base = if endpoint.trim().is_empty() {
        DEFAULT_WTTR_IN.to_string()
    } else {
        endpoint.trim().trim_end_matches('/').to_string()
    };
    let mut url = format!(
        "{base}/{:.4},{:.4}?format=j1",
        place.latitude, place.longitude
    );
    if let Some(k) = key.filter(|k| !k.is_empty()) {
        url.push_str(&format!("&apikey={}", urlencoding(k)));
    }
    url
}

/// Parse the Open-Meteo payload into the bounded shape this module answers
/// from. Provider PROSE is never read: conditions come from `weather_code`.
pub(super) fn parse_reading(body: &Value, days: u8) -> Result<Reading, WeatherError> {
    let cur = &body["current"];
    let Some(temp) = cur["temperature_2m"].as_f64() else {
        return Err(WeatherError::NoData);
    };
    let Some(code) = cur["weather_code"].as_i64() else {
        return Err(WeatherError::NoData);
    };
    let daily = &body["daily"];
    let dates = daily["time"].as_array().cloned().unwrap_or_default();
    let maxes = daily["temperature_2m_max"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mins = daily["temperature_2m_min"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let codes = daily["weather_code"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let probs = daily["precipitation_probability_max"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut rows = Vec::new();
    for i in 0..dates.len().min(days as usize) {
        let (Some(date), Some(max), Some(min), Some(code)) = (
            dates[i].as_str(),
            maxes.get(i).and_then(Value::as_f64),
            mins.get(i).and_then(Value::as_f64),
            codes.get(i).and_then(Value::as_i64),
        ) else {
            continue;
        };
        rows.push(Day {
            date: cap(date, 20),
            condition: condition_word(code),
            max_c: max,
            min_c: min,
            precip_prob_pct: probs.get(i).and_then(Value::as_i64),
        });
    }
    if rows.is_empty() {
        tracing::info!("weather: provider returned no usable daily rows");
    }
    Ok(Reading {
        fetched_ms: 0,
        provider: "open_meteo",
        observed_at: cap(cur["time"].as_str().unwrap_or(""), 40),
        temperature_c: temp,
        apparent_c: cur["apparent_temperature"].as_f64(),
        humidity_pct: cur["relative_humidity_2m"].as_i64(),
        wind_kmh: cur["wind_speed_10m"].as_f64(),
        precipitation_mm: cur["precipitation"].as_f64(),
        condition: condition_word(code),
        days: rows,
    })
}

/// Parse MET Norway Locationforecast 2.0 (`compact` / `complete`) GeoJSON.
/// Conditions come strictly from `symbol_code` mapped through `met_no_condition`.
pub(super) fn parse_met_no(body: &Value, days: u8) -> Result<Reading, WeatherError> {
    let Some(series) = body["properties"]["timeseries"].as_array() else {
        return Err(WeatherError::NoData);
    };
    let Some(first) = series.first() else {
        return Err(WeatherError::NoData);
    };
    let details = &first["data"]["instant"]["details"];
    let Some(temp) = details["air_temperature"].as_f64() else {
        return Err(WeatherError::NoData);
    };
    let humidity_pct = details["relative_humidity"]
        .as_f64()
        .map(|v| v.round() as i64);
    let wind_kmh = details["wind_speed"].as_f64().map(|mps| round1(mps * 3.6));
    let (symbol, precip_mm) = met_no_window(&first["data"]);
    let condition = met_no_condition(symbol);
    let observed_at = cap(first["time"].as_str().unwrap_or(""), 40);

    // Group hourly/6-hourly timeseries entries by calendar date (YYYY-MM-DD).
    let mut day_dates: Vec<String> = Vec::new();
    let mut day_stats: Vec<(f64, f64, &'static str, bool)> = Vec::new();
    for item in series {
        let Some(ts) = item["time"].as_str() else {
            continue;
        };
        let Some(date) = ts.get(..10) else {
            continue;
        };
        let Some(t) = item["data"]["instant"]["details"]["air_temperature"].as_f64() else {
            continue;
        };
        let (sym, _) = met_no_window(&item["data"]);
        let cond = met_no_condition(sym);
        let is_midday = ts.contains("T06:00") || ts.contains("T12:00");
        if let Some(idx) = day_dates.iter().position(|d| d == date) {
            let entry = &mut day_stats[idx];
            if t > entry.0 {
                entry.0 = t;
            }
            if t < entry.1 {
                entry.1 = t;
            }
            if (is_midday && !entry.3 && cond != "未知天气")
                || (entry.2 == "未知天气" && cond != "未知天气")
            {
                entry.2 = cond;
                entry.3 = is_midday;
            }
        } else {
            day_dates.push(date.to_string());
            day_stats.push((t, t, cond, is_midday));
        }
    }
    let mut rows = Vec::new();
    for (date, (max_c, min_c, cond, _)) in day_dates
        .into_iter()
        .zip(day_stats.into_iter())
        .take(days as usize)
    {
        rows.push(Day {
            date: cap(&date, 20),
            condition: cond,
            max_c,
            min_c,
            precip_prob_pct: None,
        });
    }

    Ok(Reading {
        fetched_ms: 0,
        provider: "met_no",
        observed_at,
        temperature_c: temp,
        apparent_c: None,
        humidity_pct,
        wind_kmh,
        precipitation_mm: precip_mm,
        condition,
        days: rows,
    })
}

fn met_no_window(data: &Value) -> (&str, Option<f64>) {
    for key in ["next_1_hours", "next_6_hours", "next_12_hours"] {
        let block = &data[key];
        if let Some(sym) = block["summary"]["symbol_code"].as_str() {
            let precip = block["details"]["precipitation_amount"].as_f64();
            return (sym, precip);
        }
    }
    ("", None)
}

/// Parse `wttr.in/?format=j1` JSON. Conditions come strictly from integer
/// `weatherCode` via `wttr_condition`, never from `weatherDesc` prose.
pub(super) fn parse_wttr_in(body: &Value, days: u8) -> Result<Reading, WeatherError> {
    let Some(cur) = body["current_condition"].as_array().and_then(|a| a.first()) else {
        return Err(WeatherError::NoData);
    };
    let Some(temp) = num_f64(&cur["temp_C"]) else {
        return Err(WeatherError::NoData);
    };
    let Some(code) = num_i64(&cur["weatherCode"]) else {
        return Err(WeatherError::NoData);
    };
    let observed_at = cap(
        cur["localObsDateTime"]
            .as_str()
            .or_else(|| cur["observation_time"].as_str())
            .unwrap_or(""),
        40,
    );
    let mut rows = Vec::new();
    if let Some(list) = body["weather"].as_array() {
        for day in list.iter().take(days as usize) {
            let (Some(date), Some(max_c), Some(min_c)) = (
                day["date"].as_str(),
                num_f64(&day["maxtempC"]),
                num_f64(&day["mintempC"]),
            ) else {
                continue;
            };
            let (day_code, precip_prob_pct) = wttr_day_summary(day, code);
            rows.push(Day {
                date: cap(date, 20),
                condition: wttr_condition(day_code),
                max_c,
                min_c,
                precip_prob_pct,
            });
        }
    }
    Ok(Reading {
        fetched_ms: 0,
        provider: "wttr_in",
        observed_at,
        temperature_c: temp,
        apparent_c: num_f64(&cur["FeelsLikeC"]),
        humidity_pct: num_i64(&cur["humidity"]),
        wind_kmh: num_f64(&cur["windspeedKmph"]),
        precipitation_mm: num_f64(&cur["precipMM"]),
        condition: wttr_condition(code),
        days: rows,
    })
}

fn wttr_day_summary(day: &Value, fallback_code: i64) -> (i64, Option<i64>) {
    let Some(hourly) = day["hourly"].as_array() else {
        return (fallback_code, None);
    };
    let mut chosen_code = None;
    let mut max_prob: Option<i64> = None;
    for slot in hourly {
        if let Some(p) = num_i64(&slot["chanceofrain"]) {
            max_prob = Some(max_prob.map_or(p, |cur| cur.max(p)));
        }
        let time = slot["time"].as_str().unwrap_or("");
        if time == "1200" || chosen_code.is_none() {
            if let Some(c) = num_i64(&slot["weatherCode"]) {
                chosen_code = Some(c);
            }
        }
    }
    (chosen_code.unwrap_or(fallback_code), max_prob)
}

fn num_f64(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
}

fn num_i64(v: &Value) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse::<i64>().ok()))
}

/// 中国天气网's JSON → the same Reading shape. `type` is already Chinese; only
/// the known vocabulary is passed through, anything else is 未知天气.
pub(super) fn parse_itboy(body: &Value, days: u8) -> Result<Reading, WeatherError> {
    if body["status"].as_i64() != Some(200) {
        return Err(WeatherError::NoData);
    }
    let data = &body["data"];
    let temp = data["wendu"]
        .as_str()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .ok_or(WeatherError::NoData)?;
    let humidity = data["shidu"]
        .as_str()
        .and_then(|v| v.trim().trim_end_matches('%').parse::<i64>().ok());
    let today = data["forecast"]
        .as_array()
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or(Value::Null);
    let stamp = body["time"].as_str().unwrap_or("");
    let date = stamp.get(..10).unwrap_or(stamp);
    let observed_at = format!(
        "{} {}",
        date,
        body["cityInfo"]["updateTime"].as_str().unwrap_or("")
    )
    .trim()
    .to_string();
    let mut rows = Vec::new();
    if let Some(list) = data["forecast"].as_array() {
        for day in list.iter().take(days as usize) {
            let (Some(date), Some(max), Some(min)) = (
                day["ymd"].as_str(),
                day["high"].as_str().and_then(temp_from_range),
                day["low"].as_str().and_then(temp_from_range),
            ) else {
                continue;
            };
            rows.push(Day {
                date: cap(date, 20),
                condition: cn_condition(day["type"].as_str().unwrap_or("")),
                max_c: max,
                min_c: min,
                precip_prob_pct: None,
            });
        }
    }
    Ok(Reading {
        fetched_ms: 0,
        provider: "weather_com_cn",
        observed_at: cap(&observed_at, 40),
        temperature_c: temp,
        apparent_c: None,
        humidity_pct: humidity,
        wind_kmh: None,
        precipitation_mm: None,
        condition: cn_condition(today["type"].as_str().unwrap_or("")),
        days: rows,
    })
}

fn temp_from_range(s: &str) -> Option<f64> {
    let cleaned: String = s
        .chars()
        .skip_while(|c| !c.is_ascii_digit() && *c != '-' && *c != '+')
        .take_while(|c| c.is_ascii_digit() || *c == '-' || *c == '.' || *c == '+')
        .collect();
    cleaned.parse::<f64>().ok()
}

pub(super) fn cn_condition(word: &str) -> &'static str {
    match word.trim() {
        "晴" => "晴",
        "多云" => "多云",
        "阴" => "阴",
        "小雨" => "小雨",
        "中雨" => "中雨",
        "大雨" => "大雨",
        "暴雨" => "暴雨",
        "阵雨" => "阵雨",
        "雷阵雨" => "雷阵雨",
        "雨夹雪" => "雨夹雪",
        "小雪" => "小雪",
        "中雪" => "中雪",
        "大雪" => "大雪",
        "雾" => "有雾",
        "霾" => "霾",
        _ => "未知天气",
    }
}

pub(super) fn condition_word(code: i64) -> &'static str {
    match code {
        0 => "晴",
        1 => "大致晴朗",
        2 => "多云",
        3 => "阴",
        45 | 48 => "有雾",
        51 | 53 | 55 => "毛毛雨",
        56 | 57 => "冻毛毛雨",
        61 => "小雨",
        63 => "中雨",
        65 => "大雨",
        66 | 67 => "冻雨",
        71 => "小雪",
        73 => "中雪",
        75 => "大雪",
        77 => "雪粒",
        80 | 81 => "阵雨",
        82 => "强阵雨",
        85 | 86 => "阵雪",
        95 => "雷阵雨",
        96 | 99 => "雷阵雨伴冰雹",
        _ => "未知天气",
    }
}

pub(super) fn met_no_condition(symbol: &str) -> &'static str {
    let base = symbol
        .trim()
        .trim_end_matches("_day")
        .trim_end_matches("_night")
        .trim_end_matches("_polartwilight");
    match base {
        "clearsky" => "晴",
        "fair" => "大致晴朗",
        "partlycloudy" => "多云",
        "cloudy" => "阴",
        "fog" => "有雾",
        "lightrain" => "小雨",
        "rain" => "中雨",
        "heavyrain" => "大雨",
        "lightrainshowers" | "rainshowers" => "阵雨",
        "heavyrainshowers" => "强阵雨",
        "lightsnow" => "小雪",
        "snow" => "中雪",
        "heavysnow" => "大雪",
        "lightsnowshowers" | "snowshowers" | "heavysnowshowers" => "阵雪",
        "sleet" | "lightsleet" | "heavysleet" | "sleetshowers" | "lightsleetshowers"
        | "heavysleetshowers" => "雨夹雪",
        "rainandthunder"
        | "lightrainandthunder"
        | "heavyrainandthunder"
        | "rainshowersandthunder"
        | "lightrainshowersandthunder"
        | "heavyrainshowersandthunder"
        | "sleetandthunder"
        | "lightsleetandthunder"
        | "heavysleetandthunder"
        | "sleetshowersandthunder"
        | "lightsleetshowersandthunder"
        | "heavysleetshowersandthunder"
        | "snowandthunder"
        | "lightsnowandthunder"
        | "heavysnowandthunder"
        | "snowshowersandthunder"
        | "lightsnowshowersandthunder"
        | "heavysnowshowersandthunder" => "雷阵雨",
        _ => "未知天气",
    }
}

pub(super) fn wttr_condition(code: i64) -> &'static str {
    match code {
        113 => "晴",
        116 => "多云",
        119 | 122 => "阴",
        143 | 248 | 260 => "有雾",
        176 | 263 | 266 | 293 | 296 => "小雨",
        299 | 302 => "中雨",
        305 | 308 => "大雨",
        185 | 281 | 284 | 311 | 314 => "冻雨",
        182 | 317 | 320 | 350 | 362 | 365 | 374 | 377 => "雨夹雪",
        179 | 323 | 326 | 368 => "小雪",
        329 | 332 => "中雪",
        227 | 230 | 335 | 338 | 371 => "大雪",
        353 => "阵雨",
        356 | 359 => "强阵雨",
        200 | 386 | 389 | 392 | 395 => "雷阵雨",
        _ => "未知天气",
    }
}

struct BuiltinCity {
    name: &'static str,
    admin: &'static str,
    pinyin: &'static str,
    lat: f64,
    lon: f64,
    city_code: &'static str,
}

const BUILTIN_CN_CITIES: &[BuiltinCity] = &[
    BuiltinCity {
        name: "沈阳",
        admin: "辽宁省",
        pinyin: "shenyang",
        lat: 41.7922,
        lon: 123.4328,
        city_code: "101070101",
    },
    BuiltinCity {
        name: "大连",
        admin: "辽宁省",
        pinyin: "dalian",
        lat: 38.9140,
        lon: 121.6147,
        city_code: "101070201",
    },
    BuiltinCity {
        name: "北京",
        admin: "北京市",
        pinyin: "beijing",
        lat: 39.9042,
        lon: 116.4074,
        city_code: "101010100",
    },
    BuiltinCity {
        name: "上海",
        admin: "上海市",
        pinyin: "shanghai",
        lat: 31.2304,
        lon: 121.4737,
        city_code: "101020100",
    },
    BuiltinCity {
        name: "广州",
        admin: "广东省",
        pinyin: "guangzhou",
        lat: 23.1291,
        lon: 113.2644,
        city_code: "101280101",
    },
    BuiltinCity {
        name: "深圳",
        admin: "广东省",
        pinyin: "shenzhen",
        lat: 22.5431,
        lon: 114.0579,
        city_code: "101280601",
    },
    BuiltinCity {
        name: "杭州",
        admin: "浙江省",
        pinyin: "hangzhou",
        lat: 30.2741,
        lon: 120.1551,
        city_code: "101210101",
    },
    BuiltinCity {
        name: "南京",
        admin: "江苏省",
        pinyin: "nanjing",
        lat: 32.0603,
        lon: 118.7969,
        city_code: "101190101",
    },
    BuiltinCity {
        name: "苏州",
        admin: "江苏省",
        pinyin: "suzhou",
        lat: 31.2990,
        lon: 120.5853,
        city_code: "101190401",
    },
    BuiltinCity {
        name: "武汉",
        admin: "湖北省",
        pinyin: "wuhan",
        lat: 30.5928,
        lon: 114.3055,
        city_code: "101200101",
    },
    BuiltinCity {
        name: "成都",
        admin: "四川省",
        pinyin: "chengdu",
        lat: 30.5728,
        lon: 104.0668,
        city_code: "101270101",
    },
    BuiltinCity {
        name: "重庆",
        admin: "重庆市",
        pinyin: "chongqing",
        lat: 29.5630,
        lon: 106.5516,
        city_code: "101040100",
    },
    BuiltinCity {
        name: "西安",
        admin: "陕西省",
        pinyin: "xian",
        lat: 34.3416,
        lon: 108.9398,
        city_code: "101110101",
    },
    BuiltinCity {
        name: "天津",
        admin: "天津市",
        pinyin: "tianjin",
        lat: 39.0842,
        lon: 117.2010,
        city_code: "101030100",
    },
    BuiltinCity {
        name: "长沙",
        admin: "湖南省",
        pinyin: "changsha",
        lat: 28.2282,
        lon: 112.9388,
        city_code: "101250101",
    },
    BuiltinCity {
        name: "郑州",
        admin: "河南省",
        pinyin: "zhengzhou",
        lat: 34.7466,
        lon: 113.6253,
        city_code: "101180101",
    },
    BuiltinCity {
        name: "青岛",
        admin: "山东省",
        pinyin: "qingdao",
        lat: 36.0671,
        lon: 120.3826,
        city_code: "101120201",
    },
    BuiltinCity {
        name: "济南",
        admin: "山东省",
        pinyin: "jinan",
        lat: 36.6512,
        lon: 117.1201,
        city_code: "101120101",
    },
    BuiltinCity {
        name: "哈尔滨",
        admin: "黑龙江省",
        pinyin: "harbin",
        lat: 45.8038,
        lon: 126.5350,
        city_code: "101050101",
    },
    BuiltinCity {
        name: "长春",
        admin: "吉林省",
        pinyin: "changchun",
        lat: 43.8171,
        lon: 125.3235,
        city_code: "101060101",
    },
    BuiltinCity {
        name: "福州",
        admin: "福建省",
        pinyin: "fuzhou",
        lat: 26.0745,
        lon: 119.2965,
        city_code: "101230101",
    },
    BuiltinCity {
        name: "厦门",
        admin: "福建省",
        pinyin: "xiamen",
        lat: 24.4798,
        lon: 118.0894,
        city_code: "101230201",
    },
    BuiltinCity {
        name: "合肥",
        admin: "安徽省",
        pinyin: "hefei",
        lat: 31.8206,
        lon: 117.2272,
        city_code: "101220101",
    },
    BuiltinCity {
        name: "昆明",
        admin: "云南省",
        pinyin: "kunming",
        lat: 25.0389,
        lon: 102.7183,
        city_code: "101290101",
    },
];

pub(super) fn builtin_city_code(alias: &str, name: &str) -> Option<&'static str> {
    for candidate in [alias.trim(), name.trim()] {
        if candidate.is_empty() {
            continue;
        }
        let clean = candidate.trim_end_matches('市');
        for city in BUILTIN_CN_CITIES {
            if clean == city.name
                || candidate.starts_with(city.name)
                || candidate.eq_ignore_ascii_case(city.pinyin)
            {
                return Some(city.city_code);
            }
        }
    }
    None
}

pub(super) fn builtin_geocode(query: &str) -> Vec<Value> {
    let q = query.trim().trim_end_matches('市').to_ascii_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    BUILTIN_CN_CITIES
        .iter()
        .filter(|c| {
            c.name.contains(q.as_str())
                || q.contains(c.name)
                || c.admin.contains(q.as_str())
                || c.pinyin.starts_with(q.as_str())
        })
        .take(5)
        .map(|c| {
            json!({
                "name": c.name,
                "admin": c.admin,
                "country": "中国",
                "label": format!("{} · {} · 中国", c.name, c.admin),
                "latitude": c.lat,
                "longitude": c.lon,
                "timezone": "Asia/Shanghai",
                "city_code": c.city_code,
            })
        })
        .collect()
}

pub(super) fn cap(s: &str, max: usize) -> String {
    s.chars().filter(|c| !c.is_control()).take(max).collect()
}

pub(super) fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

pub(super) fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
