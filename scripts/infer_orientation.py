#!/usr/bin/env python3
import sqlite3
import json
import math
import sys
from datetime import datetime, timezone

def calculate_solar_position(lat, lon, dt):
    lat_rad = math.radians(lat)
    d = dt.timetuple().tm_yday
    delta = math.radians(23.45) * math.sin(2.0 * math.pi * (284.0 + d) / 365.0)
    b = math.radians(360.0 * (d - 81.0) / 364.0)
    eot = 9.87 * math.sin(2.0 * b) - 7.53 * math.cos(b) - 1.5 * math.sin(b)
    utc_hour = dt.hour + dt.minute / 60.0 + dt.second / 3600.0
    solar_time = utc_hour + lon / 15.0 + eot / 60.0
    h = math.radians(15.0 * (solar_time - 12.0))
    sin_alpha = math.sin(lat_rad) * math.sin(delta) + math.cos(lat_rad) * math.cos(delta) * math.cos(h)
    alpha = math.asin(sin_alpha)
    cos_alpha = math.cos(alpha)
    if abs(cos_alpha) > 1e-6:
        cos_theta_s = (math.sin(delta) * math.cos(lat_rad) - math.cos(delta) * math.sin(lat_rad) * math.cos(h)) / cos_alpha
        cos_theta_s = max(-1.0, min(1.0, cos_theta_s))
        sin_theta_s = -math.cos(delta) * math.sin(h) / cos_alpha
        sin_theta_s = max(-1.0, min(1.0, sin_theta_s))
        theta_s = math.atan2(sin_theta_s, cos_theta_s)
    else:
        theta_s = 0.0
    return alpha, theta_s

def calculate_poa_irradiance(dni, dhi, solar_elevation, solar_azimuth, tilt_deg, azimuth_deg):
    if solar_elevation <= 0.0:
        return 0.0
    tilt_rad = math.radians(tilt_deg)
    azimuth_rad = math.radians(azimuth_deg)
    cos_incidence = math.sin(solar_elevation) * math.cos(tilt_rad) + \
                    math.cos(solar_elevation) * math.sin(tilt_rad) * math.cos(solar_azimuth - azimuth_rad)
    direct_poa = dni * max(0.0, cos_incidence)
    diffuse_poa = dhi * (1.0 + math.cos(tilt_rad)) / 2.0
    return direct_poa + diffuse_poa

def pearson_correlation(x, y):
    n = len(x)
    if n == 0:
        return -1.0
    sum_x = sum(x)
    sum_y = sum(y)
    sum_x2 = sum(xi * xi for xi in x)
    sum_y2 = sum(yi * yi for yi in y)
    sum_xy = sum(xi * yi for xi, yi in zip(x, y))
    num = n * sum_xy - sum_x * sum_y
    den = math.sqrt((n * sum_x2 - sum_x * sum_x) * (n * sum_y2 - sum_y * sum_y))
    return num / den if den > 1e-9 else -1.0

def run_inference(db_path="config.db"):
    print(f"Connecting to database: {db_path}...")
    try:
        conn = sqlite3.connect(db_path)
    except Exception as e:
        print(f"Error opening database: {e}")
        sys.exit(1)

    # 1. Load coordinates from settings
    try:
        row = conn.execute("SELECT config_json FROM settings WHERE id = 1").fetchone()
        if not row:
            print("No settings record found in database.")
            sys.exit(1)
        config = json.loads(row[0])
        location = config.get("Location", {})
        lat = location.get("latitude")
        lon = location.get("longitude")
        if lat is None or lon is None:
            print("Latitude and longitude coordinates are not configured in settings.")
            sys.exit(1)
    except Exception as e:
        print(f"Error loading location config: {e}")
        sys.exit(1)

    print(f"Configured coordinates: Latitude = {lat}, Longitude = {lon}")

    # 2. Get active PV strings topics from telemetry_history
    try:
        topics = [r[0] for r in conn.execute(
            "SELECT DISTINCT topic FROM telemetry_history WHERE topic LIKE '%/PV1 Power' OR topic LIKE '%/PV2 Power'"
        ).fetchall()]
    except Exception as e:
        print(f"Error listing telemetry topics: {e}")
        sys.exit(1)

    if not topics:
        print("No PV string telemetry found in telemetry_history table.")
        sys.exit(0)

    print(f"Found active PV strings: {topics}")
    thirty_days_ago = int(datetime.now().timestamp()) - (30 * 24 * 3600)

    for topic in topics:
        print(f"\nAnalyzing topic: {topic}")
        rows = conn.execute(
            "SELECT timestamp, value FROM telemetry_history WHERE topic = ? AND timestamp >= ? ORDER BY timestamp ASC",
            (topic, thirty_days_ago)
        ).fetchall()

        if not rows:
            print("No telemetry data in the past 30 days.")
            continue

        # Group by day
        daily_data = {}
        for ts, val in rows:
            day = ts // 86400
            if day not in daily_data:
                daily_data[day] = []
            daily_data[day].append((ts, val))

        # Find top 5 days by total generation
        daily_totals = []
        for day, pts in daily_data.items():
            total = sum(v for _, v in pts)
            daily_totals.append((day, total))
        daily_totals.sort(key=lambda x: x[1], reverse=True)
        top_days = [day for day, _ in daily_totals[:5]]

        # Gather daytime points
        points = []
        for day in top_days:
            for ts, val in daily_data[day]:
                if val > 10.0:
                    points.append((ts, val))

        if len(points) < 24:
            print(f"Insufficient daytime points ({len(points)} < 24) on clear-sky days to run inference.")
            continue

        # Precompute solar position
        precomputed = []
        for ts, actual_w in points:
            dt = datetime.fromtimestamp(ts, tz=timezone.utc)
            el, az = calculate_solar_position(lat, lon, dt)
            precomputed.append((actual_w, el, az))

        # Coarse Grid Search
        best_r = -2.0
        best_tilt = 20.0
        best_azimuth = 180.0

        for t_deg in range(0, 61, 2):
            for a_deg in range(0, 360, 5):
                t = float(t_deg)
                a = float(a_deg)
                actual_vals = [p[0] for p in precomputed]
                modeled_vals = [calculate_poa_irradiance(800.0, 100.0, p[1], p[2], t, a) for p in precomputed]
                r = pearson_correlation(actual_vals, modeled_vals)
                if r > best_r:
                    best_r = r; best_tilt = t; best_azimuth = a

        # Fine Grid Search
        coarse_tilt = best_tilt
        coarse_azimuth = best_azimuth
        fine_best_r = best_r

        for t_diff in range(-10, 11):
            t = coarse_tilt + (t_diff * 0.5)
            if t < 0.0 or t > 90.0:
                continue
            for a_diff in range(-10, 11):
                a = (coarse_azimuth + (a_diff * 0.5) + 360.0) % 360.0
                actual_vals = [p[0] for p in precomputed]
                modeled_vals = [calculate_poa_irradiance(800.0, 100.0, p[1], p[2], t, a) for p in precomputed]
                r = pearson_correlation(actual_vals, modeled_vals)
                if r > fine_best_r:
                    fine_best_r = r; best_tilt = t; best_azimuth = a

        print(f"Results for {topic}:")
        print(f"  Estimated Tilt:    {best_tilt:.1f}°")
        print(f"  Estimated Azimuth: {best_azimuth:.1f}°")
        print(f"  Correlation Score: {fine_best_r:.4f}")

if __name__ == "__main__":
    db = "config.db"
    if len(sys.argv) > 1:
        db = sys.argv[1]
    run_inference(db)
