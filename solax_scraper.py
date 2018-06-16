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

import pprint
pp = pprint.PrettyPrinter(indent=4)

import traceback

from Inputs.SolaxWifi import SolaxWifi
from Inputs.SolaxModbus import SolaxModbus
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
    
    
def inputActions(solaxWifiInverters, solaxModbusInverters):
    for inverter in solaxWifiInverters:
        inverter.fetch(outputActions)
        
    for inverter in solaxModbusInverters:
        inverter.fetch(outputActions)

            
globalLogPublisher.addObserver(analyze)

with open("config.toml") as conffile:
    global config
    config = toml.loads(conffile.read())
    
#     pp.pprint(config)

global emonCMS
if 'emoncms' in config:
    emonCMS = EmonCMS(config['emoncms'])

solaxWifiInverters = []
for inverter in config['Solax-Wifi']['inverters']:
    wifiInverter = SolaxWifi(inverter, config['solax-Wifi']['timeout'])
    solaxWifiInverters.append(wifiInverter)

solaxModbusInverters = []
for inverter in config['Solax-Modbus']['inverters']:
    modbusInverter = SolaxModbus(inverter)
    solaxModbusInverters.append(modbusInverter)


looper = task.LoopingCall(inputActions, solaxWifiInverters, solaxModbusInverters)
looper.start(config['poll_period'])

reactor.run()
