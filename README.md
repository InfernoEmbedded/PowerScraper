# PowerScraper
A scraper for power devices to feed data to OpenEnergyMonitor

# Supported Hardware
- Solax SK-SU5000E Wifi interface (Solax-Wifi)
- Solax SK-SU5000E Modbus/TCP Interface (Solax-Modbus)
- Solax X1 & X3 Standalone inverters (SolaxX3RS485)
- Solax X1 & X3 series hybrid inverters (SolaxXHybrid)
- Solax X1 & X3 series retrofit chargers (SolaxXHybrid)
- Eastron SDM630V2 3 Phase energy meter (SDB630ModbusV2)

# Supported Outputs
- Open Energy Monitor (EmonCMS)
- InfluxDB

# Prerequisites
```pip3 install pymodbus Twisted influxdb-client```

