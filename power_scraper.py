#!/usr/bin/python3

# Scrapes Inverter information from solax inverters and presents it to OpenEnergyMonitor
#
# Setup:
#   pip install toml twisted pymodbus
#   cp config-example.toml config.toml
#   vi config.toml 
#
# Copyright (c)2018 Inferno Embedded   http://infernoembedded.com
# 
# This program is free software: you can redistribute it and/or modify
#    it under the terms of the GNU General Public License as published by
#    the Free Software Foundation, either version 3 of the License, or
#    (at your option) any later version.
# 
#    This program is distributed in the hope that it will be useful,
#    but WITHOUT ANY WARRANTY; without even the implied warranty of
#    MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
#    GNU General Public License for more details.
# 
#    You should have received a copy of the GNU General Public License
#    along with this program.  If not, see <https://www.gnu.org/licenses/>.

import toml
from twisted.internet import task, reactor
#from twisted.internet.protocol import Protocol
from twisted.logger import globalLogPublisher

import logging
logging.basicConfig()
# log = logging.getLogger()
# log.setLevel(logging.DEBUG)

import traceback

from Inputs.SolaxWifi import SolaxWifi
from Inputs.SolaxModbus import SolaxModbus
from Inputs.SDM630ModbusV2 import SDM630ModbusV2
from Outputs.EmonCMS import EmonCMS

from twisted.internet.defer import setDebugging
setDebugging(True)

from twisted.logger._levels import LogLevel

def analyze(event):
    if event.get("log_level") == LogLevel.critical:
        print("Critical: ", event)
               
def outputActions(vals):
    if emonCMS is not None:
        emonCMS.send(vals)
    
    
def inputActions(inputs):
    for input in inputs:
        input.fetch(outputActions)
            
globalLogPublisher.addObserver(analyze)

with open("config.toml") as conffile:
    global config
    config = toml.loads(conffile.read())
    
#     pp.pprint(config)

global emonCMS
if 'emoncms' in config:
    emonCMS = EmonCMS(config['emoncms'])

if 'Solax-Wifi' in config:
    SolaxWifiInverters = []
    for inverter in config['Solax-Wifi']['inverters']:
        wifiInverter = SolaxWifi(inverter, config['solax-Wifi']['timeout'])
        SolaxWifiInverters.append(wifiInverter)

    looperSolaxWifi = task.LoopingCall(inputActions, SolaxWifiInverters)
    looperSolaxWifi.start(config['Solax-Wifi']['poll_period'])

if 'Solax-Modbus' in config:
    SolaxModbusInverters = []
    for inverter in config['Solax-Modbus']['inverters']:
        modbusInverter = SolaxModbus(inverter)
        SolaxModbusInverters.append(modbusInverter)

    looperSolaxModbus = task.LoopingCall(inputActions, SolaxModbusInverters)
    looperSolaxModbus.start(config['Solax-Modbus']['poll_period'])

if 'SDM630ModbusV2' in config:
    SDM630Meters = []
    for meter in config['SDM630ModbusV2']['ports']:
        modbusMeter = SDM630ModbusV2(meter, config['SDM630ModbusV2']['baud'], config['SDM630ModbusV2']['parity'],
                                 config['SDM630ModbusV2']['stopbits'], config['SDM630ModbusV2']['timeout'])
        SDM630Meters.append(modbusMeter)

    looperSDM630 = task.LoopingCall(inputActions, SDM630Meters)
    looperSDM630.start(config['SDM630ModbusV2']['poll_period'])

reactor.run()
