#!/usr/bin/python3

# Scrapes Inverter information from Solax inverters and presents it to OpenEnergyMonitor
#
# Setup:
#   pip install toml twisted
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
from Outputs.EmonCMS import EmonCMS

from twisted.internet.defer import setDebugging
setDebugging(True)

from twisted.logger._levels import LogLevel

def analyze(event):
    if event.get("log_level") == LogLevel.critical:
        print("Critical: ", event)
               
def outputActions(vals):
    print("Outputactions\n")
    if emonCMS is not None:
        emonCMS.send(vals)
    
    
def inputActions(SolaxWifiInverters):
    requests = []
    
    for inverter in SolaxWifiInverters:
        request = inverter.fetch(outputActions)
        requests.append(request)
            
globalLogPublisher.addObserver(analyze)

with open("config.toml") as conffile:
    global config
    config = toml.loads(conffile.read())
    
#     pp.pprint(config)

global emonCMS
if 'emoncms' in config:
    emonCMS = EmonCMS(config['emoncms'])

SolaxWifiInverters = []
for inverter in config['Solax-Wifi']['inverters']:
    wifiInverter = SolaxWifi(inverter, config['Solax-Wifi']['timeout'])
    SolaxWifiInverters.append(wifiInverter)

looper = task.LoopingCall(inputActions, SolaxWifiInverters)
looper.start(config['poll_period'])

reactor.run()
