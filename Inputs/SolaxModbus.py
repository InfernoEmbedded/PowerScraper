from pymodbus.factory import ClientDecoder
from pymodbus.client.asynchronous.twisted import ModbusClientProtocol
from pymodbus.framer.rtu_framer import ModbusRtuFramer

from twisted.internet import defer, reactor, protocol
from twisted.web.client import Agent, readBody
from twisted.web.http_headers import Headers

import datetime
#import unicodedata

#import pprint
#pp = pprint.PrettyPrinter(indent=4)


def unsigned16(result, addr):
    return result.getRegister(addr)

def unsigned32(result, addr):
    low = result.getRegister(addr)
    high = result.getRegister(addr + 1)
    val = low + (high << 16)

    return val

def signed16(result, addr):
    val = result.getRegister(addr)

    if val > 32767:
        val -= 65535
    return val

def signed32(result, addr):
    val = unsigned32(result, addr)

    if val > 2147483647:
        val -= 4294967295
    return val


class SolaxProtocol(ModbusClientProtocol):
    def connectionMade(self):
        ModbusClientProtocol.connectionMade(self);
        self.factory.setClient(self)


class SolaxFactory(protocol.ReconnectingClientFactory):
    protocol = SolaxProtocol
    client = None
    config = None
    ready = False

    def __init__(self, config):
        self.config = config

    def err(self, arg):
        print('err', arg)

    def setClient(self, client):
        self.client = client

        if 'installer_password' in self.config['Solax-Modbus']:
            self.ready = False
            result = client.write_register(0x00, self.config['Solax-Modbus']['installer_password'])
            if result != None:
                result.addCallback(self.markReady)
        else:
            self.ready = True

    def getClient(self):
        return self.client

    def markReady(self, result):
        self.ready = True

    def readRegisters(self):
        if self.ready:
            return self.client.read_input_registers(0, 0x72)
        else:
            print("not connected")

    def buildProtocol(self, addr):
        self.resetDelay()
        p = SolaxProtocol()
        p.factory = self
        return p

    def shutdown(self):
        self.doStop()

class SolaxModbus(object):
    host = None
    config = None
    factory = None
    requestedBatteryPower = 0
    powerBudgets = []

    ##
    # Create a new class to fetch data from the Modbus interface of Solax inverters
    def __init__(self, config, host):
        self.host = host
        self.config = config
        self.factory = SolaxFactory(config)
        reactor.connectTCP(host, 502, self.factory)

    def err(self, arg):
        print('err', arg)

    def fetch(self, completionCallback):
        result = self.factory.readRegisters()
        if result != None:
            result.addCallback(self.solaxRegisterCallback, completionCallback)
            result.addErrback(self.err)

    def getPeriod(self):
        nowDateTime = datetime.datetime.now()
        now = datetime.time(nowDateTime.hour, nowDateTime.minute, nowDateTime.second)

        for periodName, period in self.config['Solax-BatteryControl']['Period'].items():
            bits = period['start'].split(':')
            start = datetime.time(int(bits[0]), int(bits[1]), int(bits[2]))

            bits = period['end'].split(':')
            end = datetime.time(int(bits[0]), int(bits[1]), int(bits[2]))

            if start < end:
                if start <= now < end:
                    return period
            else:
                if not (end <= now < start):
                    return period

        print("No period for {}\n".format(now))
        return None

    def shutdown(self):
        self.factory.shutdown()

    def solaxRegisterCallback(self, result, completionCallback):
        vals = {}
        vals['name'] = self.host;
        vals['Requested Battery Power'] = self.requestedBatteryPower
        vals['Grid Voltage'] = unsigned16(result, 0x00) / 10
        vals['Grid Current'] = signed16(result, 0x01) / 10
        vals['Inverter Power'] = signed16(result, 0x02)
        vals['PV1 Voltage'] = unsigned16(result, 0x03) / 10
        vals['PV2 Voltage'] = unsigned16(result, 0x04) / 10
        vals['PV1 Current'] = unsigned16(result, 0x05) / 10
        vals['PV2 Current'] = unsigned16(result, 0x06) / 10
        vals['Grid Frequency'] = unsigned16(result, 0x07) / 100
        vals['Inner Temp'] = signed16(result, 0x08)
        vals['Run Mode'] = unsigned16(result, 0x09)
        vals['PV1 Power'] = unsigned16(result, 0x0a)
        vals['PV2 Power'] = unsigned16(result, 0x0b)
        vals['Battery Voltage'] = signed16(result, 0x14) / 100
        vals['Battery Current'] = signed16(result, 0x15) / 100
        vals['Battery Power'] = signed16(result, 0x16)
        vals['Charger Board Temperature'] = signed16(result, 0x17)
        vals['Charger Battery Temperature'] = signed16(result, 0x18)
        vals['Charger Boost Temperature'] = signed16(result, 0x19)
        vals['Battery Capacity'] = unsigned16(result, 0x1C)

        fullLimit = 95
        if vals['name'] in self.config['Solax-BatteryControl']['Inverter']:
            inverter = self.config['Solax-BatteryControl']['Inverter'][vals['name']]
            period = self.getPeriod()
            if period is not None and 'grace' in period and 'grace-capacity' in inverter and inverter['grace-capacity'] > 0:
                fullLimit = inverter['grace-capacity']

            vals['Battery Demand'] = 0 if vals['Battery Capacity'] >= fullLimit else inverter['max-charge']

        vals['Battery Energy Discharged'] = unsigned32(result, 0x1D) / 10
        vals['BMS Warning'] = unsigned16(result, 0x1F)
        vals['Battery Energy Charged'] = unsigned32(result, 0x20) / 10
        vals['Battery State of Health'] = unsigned16(result, 0x23)
        vals['Inverter Fault'] = unsigned32(result, 0x40)
        vals['Charger Fault'] = unsigned16(result, 0x42)
        vals['Manager Fault'] = unsigned16(result, 0x43)
        vals['Measured Power'] = signed32(result, 0x46) # Power from the grid +ve, to grid -ve
        vals['Feed In Energy'] = unsigned32(result, 0x48) / 100 # Energy delivered to the grid, kWh
        vals['Consumed Energy'] = unsigned32(result, 0x4A) / 100 # Energy consumed from the grid, kWh
        vals['EPS Voltage'] = unsigned16(result, 0x4C) / 10
        vals['EPS Current'] = unsigned16(result, 0x4D) / 10
        vals['EPS VA'] = unsigned16(result, 0x4E)
        vals['EPS Frequency'] = unsigned16(result, 0x4F) / 100
        vals['Energy Today'] = unsigned16(result, 0x50) / 10
        vals['Energy Total'] = unsigned32(result, 0x52) / 1000
        vals['Battery Temperature'] = unsigned16(result, 0x55) / 10
        vals['Solar Energy Total'] = unsigned32(result, 0x70) / 10 # kWh

        vals['Power Budget'] = vals['Battery Power'] + vals['Measured Power']
        self.powerBudgets.append(vals['Power Budget'])
        if len(self.powerBudgets) > self.config['Solax-Modbus']['power_budget_avg_samples']:
            del self.powerBudgets[0]
        vals['Power Budget Average'] =  sum(self.powerBudgets) / len(self.powerBudgets)

        vals['Usage'] = vals['Inverter Power'] - vals['Measured Power']

        completionCallback(vals, self)

    def wakeupInverter(self, result):
        result = self.factory.getClient().write_register(0x90, 1)

    def chargeBattery(self, power):
        self.requestedBatteryPower = power

        # Convert to int16
        if power < 0:
            power += 65536

        result = self.factory.getClient().write_register(0x51, power)
        if power != 0:
            result.addCallback(self.wakeupInverter)
