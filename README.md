# PowerScraper
A scraper for power devices to feed data to OpenEnergyMonitor

We recommend that TCP Modbus inverters are connected through [Modbus Proxy](https://github.com/tiagocoutinho/modbus-proxy), as the inverters only allow a single connection at a time.

You can also use [ModbusGUI4Solax](https://github.com/InfernoEmbedded/ModbusGUI4Solax) to interrogate the inverters through the proxy, while PowerScraper is controlling it.

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

