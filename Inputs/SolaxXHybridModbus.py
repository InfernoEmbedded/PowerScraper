from pymodbus.factory import ClientDecoder
from pymodbus.client.asynchronous.twisted import ModbusClientProtocol
from pymodbus.framer.rtu_framer import ModbusRtuFramer

from twisted.internet import defer, reactor, protocol
from twisted.web.client import Agent, readBody
from twisted.web.http_headers import Headers

import datetime, traceback

def unsigned16(result, addr):
    val = result.getRegister(addr)
    return val

def unsigned32Split(result, addrLow, addrHigh):
    low = result.getRegister(addrLow)
    high = result.getRegister(addrHigh)
    val = low + (high << 16)

    return val

def unsigned32(result, addr):
    return unsigned32Split(result, addr, addr + 1)

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


class SolaxXHybridProtocol(ModbusClientProtocol):
    def connectionMade(self):
        ModbusClientProtocol.connectionMade(self);
        self.factory.setClient(self)

class SolaxXHybridFactory(protocol.ReconnectingClientFactory):
    protocol = SolaxXHybridProtocol
    client = None
    config = None
    ready = False

    def toJSON(self):
        return json.dumps(self, default=lambda o: o.__dict__, sort_keys=True, indent=4)

    def __init__(self, config):
        self.config = config

    def err(self, arg):
        print('err', arg)

    def setClient(self, client):
        self.client = client

        if 'installer_password' in self.config['Solax-XHybrid-Modbus']:
            self.ready = False
            result = client.write_register(0x00, self.config['Solax-XHybrid-Modbus']['installer_password'])
            if result != None:
                result.addCallback(self.setTimeout)
        else:
            self.ready = True

    def getClient(self):
        return self.client

    def setTimeout(self, result):
        result = self.client.write_register(0x9F, 30)
        if result != None:
             result.addCallback(self.enableRemoteControl)

    def enableRemoteControl(self, result):
        result = self.client.write_register(0x51, 1)
        if result != None:
             result.addCallback(self.setReactivePower)

    def setReactivePower(self, result):
        result = self.client.write_register(0x53, 0)
        if result != None:
             result.addCallback(self.allowGridCharge)

    def allowGridCharge(self, result):
        result = self.client.write_register(0x40, 3)
        if result != None:
             result.addCallback(self.markReady)

    def markReady(self, result):
        self.ready = True

    def readRegistersA(self):
        if self.ready:
            return self.client.read_input_registers(0, 0x27) # CE
        else:
            print("not connected")

    def readRegistersB(self):
        return self.client.read_input_registers(0x40, 0x69 - 0x40 + 1)

    def readRegistersC(self):
        return self.client.read_input_registers(0x6A, 0xCD - 0x6A + 1)

    def buildProtocol(self, addr):
        self.resetDelay()
        p = SolaxXHybridProtocol()
        p.factory = self
        return p

    def shutdown(self):
        self.doStop()

class SolaxXHybridModbus(object):
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
        self.factory = SolaxXHybridFactory(config)
        reactor.connectTCP(host, 502, self.factory)

    def err(self, arg):
        print('err', arg)

    def fetch(self, completionCallback):
        result = self.factory.readRegistersA()
        if result != None:
            result.addCallback(self.solaxRegisterACallback, completionCallback)
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

    def solaxRegisterACallback(self, result, completionCallback):
        self.completionCallback = completionCallback

        self.vals = {}
        self.vals['name'] = self.host;
        self.vals['Requested Battery Power'] = self.requestedBatteryPower;
        self.vals['Grid Voltage (X1)'] = unsigned16(result, 0x00) / 10
        self.vals['Grid Current (X1)'] = signed16(result, 0x01) / 10
        self.vals['Inverter Power (X1)'] = signed16(result, 0x02)
        self.vals['PV1 Voltage (Hybrid)'] = unsigned16(result, 0x03) / 10
        self.vals['PV2 Voltage (Hybrid)'] = unsigned16(result, 0x04) / 10
        self.vals['PV1 Current (Hybrid)'] = unsigned16(result, 0x05) / 10
        self.vals['PV2 Current (Hybrid)'] = unsigned16(result, 0x06) / 10
        self.vals['Grid Frequency (X1)'] = unsigned16(result, 0x07) / 100
        self.vals['Inner Temp'] = signed16(result, 0x08)
        self.vals['Run Mode'] = unsigned16(result, 0x09)
        self.vals['PV1 Power'] = unsigned16(result, 0x0a)
        self.vals['PV2 Power'] = unsigned16(result, 0x0b)
        self.vals['Battery Voltage'] = signed16(result, 0x14) / 100
        self.vals['Battery Current'] = signed16(result, 0x15) / 100
        self.vals['Battery Power'] = signed16(result, 0x16)
        self.vals['BMS Connect State'] = unsigned16(result, 0x17)
        self.vals['Battery Temperature'] = signed16(result, 0x18)
        self.vals['Charger Boost Temperature'] = signed16(result, 0x19)
        self.vals['Battery Capacity'] = unsigned16(result, 0x1C)
        self.vals['Battery Energy Discharged'] = unsigned32(result, 0x1D) / 10
        self.vals['BMS Warning'] = unsigned32Split(result, 0x1F, 0x26)
        self.vals['Battery Energy Discharged Today'] = unsigned16(result, 0x20)
        self.vals['Battery Energy Charged'] = unsigned32(result, 0x21) / 10
        self.vals['Battery Energy Charged Today'] = unsigned16(result, 0x23)
        self.vals['BMS Max Charge Current'] = unsigned16(result, 0x24) / 10
        self.vals['BMS Max Discharge Current'] = unsigned16(result, 0x25) / 10
        result2 = self.factory.readRegistersB()
        if result2 != None:
            result2.addCallback(self.solaxRegisterBCallback)
            result2.addErrback(self.err)

    def solaxRegisterBCallback(self, result):
        base = 0x40
        self.vals['Inverter Fault'] = unsigned32(result, 0x40 - base)
        self.vals['Manager Fault'] = unsigned16(result, 0x43 - base)
        self.vals['Measured Power'] = signed32(result, 0x46 - base) # Power from the grid +ve, to grid -ve
        self.vals['Feed In Energy'] = unsigned32(result, 0x48 - base) / 100 # Energy delivered to the grid, kWh
        self.vals['Consumed Energy'] = unsigned32(result, 0x4A - base) / 100 # Energy consumed from the grid, kWh
        self.vals['EPS Voltage (X1)'] = unsigned16(result, 0x4C - base) / 10
        self.vals['EPS Current (X1)'] = unsigned16(result, 0x4D - base) / 10
        self.vals['EPS VA (X1)'] = unsigned16(result, 0x4E - base)
        self.vals['EPS Frequency (X1)'] = unsigned16(result, 0x4F - base) / 100
        self.vals['Energy Today'] = unsigned16(result, 0x50 - base) / 10
        self.vals['Energy Total'] = unsigned32(result, 0x52 - base) / 1000
        self.vals['Lock State'] = unsigned16(result, 0x54 - base)
        self.vals['Bus Voltage'] = unsigned16(result, 0x66 - base) / 10
        self.vals['DC Voltage Fault'] = unsigned16(result, 0x67 - base) / 10
        self.vals['Overload Fault'] = unsigned16(result, 0x68 - base)
        self.vals['Battery Voltage Fault'] = unsigned16(result, 0x69 - base)
        result2 = self.factory.readRegistersC()
        if result2 != None:
            result2.addCallback(self.solaxRegisterCCallback)
            result2.addErrback(self.err)

    def solaxRegisterCCallback(self, result):
        base = 0x6A
        self.vals['Grid Voltage (X3) Phase 1'] = unsigned16(result, 0x6A - base) / 10
        self.vals['Grid Current (X3) Phase 1'] = unsigned16(result, 0x6B - base) / 10
        self.vals['Grid Power (X3) Phase 1'] = unsigned16(result, 0x6C - base)
        self.vals['Grid Frequency (X3) Phase 1'] = unsigned16(result, 0x6D - base) / 100
        self.vals['Grid Voltage (X3) Phase 2'] = unsigned16(result, 0x6E - base) / 10
        self.vals['Grid Current (X3) Phase 2'] = unsigned16(result, 0x6F - base) / 10
        self.vals['Grid Power (X3) Phase 2'] = unsigned16(result, 0x70 - base)
        self.vals['Grid Frequency (X3) Phase 2'] = unsigned16(result, 0x71 - base) / 100
        self.vals['Grid Voltage (X3) Phase 3'] = unsigned16(result, 0x72 - base) / 10
        self.vals['Grid Current (X3) Phase 3'] = unsigned16(result, 0x73 - base) / 10
        self.vals['Grid Power (X3) Phase 3'] = unsigned16(result, 0x74 - base)
        self.vals['Grid Frequency (X3) Phase 3'] = unsigned16(result, 0x75 - base) / 100
        self.vals['EPS Voltage (X3) Phase 1'] = unsigned16(result, 0x76 - base) / 10
        self.vals['EPS Current (X3) Phase 1'] = unsigned16(result, 0x77 - base) / 10
        self.vals['EPS Power (X3) Phase 1'] = unsigned16(result, 0x78 - base)
        self.vals['EPS VA (X3) Phase 1'] = unsigned16(result, 0x79 - base)
        self.vals['EPS Voltage (X3) Phase 2'] = unsigned16(result, 0x7A - base) / 10
        self.vals['EPS Current (X3) Phase 2'] = unsigned16(result, 0x7B - base) / 10
        self.vals['EPS Power (X3) Phase 2'] = unsigned16(result, 0x7C - base)
        self.vals['EPS VA (X3) Phase 2'] = unsigned16(result, 0x7D - base)
        self.vals['EPS Voltage (X3) Phase 3'] = unsigned16(result, 0x7E - base) / 10
        self.vals['EPS Current (X3) Phase 3'] = unsigned16(result, 0x7F - base) / 10
        self.vals['EPS Power (X3) Phase 3'] = unsigned16(result, 0x80 - base)
        self.vals['EPS VA (X3) Phase 3'] = unsigned16(result, 0x81 - base)
        self.vals['Measured Power (X3) Phase 1'] = signed32(result, 0x82 - base)
        self.vals['Measured Power (X3) Phase 2'] = signed32(result, 0x84 - base)
        self.vals['Measured Power (X3) Phase 3'] = signed32(result, 0x86 - base)
        self.vals['Grid Mode Hours (X3)'] = signed32(result, 0x88 - base) / 10
        self.vals['EPS Mode Hours (X3)'] = signed32(result, 0x8A - base) / 10
        self.vals['Normal Mode Hours (X1)'] = signed32(result, 0x8C - base) / 10
        self.vals['EPS Yield Total Energy'] = unsigned32(result, 0x8E - base) / 10
        self.vals['EPS Yield Energy Today'] = unsigned16(result, 0x90 - base) / 10
        self.vals['AC Charge Energy Today'] = unsigned16(result, 0x91 - base)
        self.vals['AC Charge Energy Total'] = unsigned32(result, 0x92 - base)
        self.vals['Solar Energy Total'] = unsigned32(result, 0x94 - base)
        self.vals['Solar Energy Today'] = unsigned16(result, 0x96 - base) / 10
        self.vals['Feedin Energy Today'] = unsigned32(result, 0x98 - base) / 10
        self.vals['Consumed Energy Today'] = unsigned32(result, 0x9A - base) / 10
        self.vals['Active Power'] = signed32(result, 0x9C - base)
        self.vals['Reactive Power'] = signed32(result, 0x9E - base)
        self.vals['Active Power Upper'] = signed32(result, 0xA0 - base)
        self.vals['Active Power Lower'] = signed32(result, 0xA2 - base)
        self.vals['Reactive Power Upper'] = signed32(result, 0xA4 - base)
        self.vals['Reactive Power Lower'] = signed32(result, 0xA6 - base)
        self.vals['Feedin Power Meter 2'] = signed32(result, 0xA8 - base)
        self.vals['Feedin Energy Total Meter 2'] = unsigned32(result, 0xAA - base) / 100
        self.vals['Consumed Energy Meter 2'] = unsigned32(result, 0xAC - base) / 100
        self.vals['Feedin Energy Today Meter 2'] = unsigned16(result, 0xAE - base) / 100
        self.vals['Consumed Energy Today Meter 2'] = unsigned16(result, 0xB0 - base) / 100
        self.vals['Feedin Power (X3) Phase 1 Meter 2'] = signed32(result, 0xB2 - base)
        self.vals['Feedin Power (X3) Phase 2 Meter 2'] = signed32(result, 0xB4 - base)
        self.vals['Feedin Power (X3) Phase 3 Meter 2'] = signed32(result, 0xB6 - base)
        self.vals['Meter 1 Communication State'] = unsigned16(result, 0xB8 - base)
        self.vals['Meter 2 Communication State'] = unsigned16(result, 0xB9 - base)
        self.vals['Grid Voltage'] = unsigned16(result, 0xBA - base) / 10
        self.vals['Grid Current'] = signed16(result, 0xBB - base) / 10
        self.vals['Grid Power'] = signed16(result, 0xBC - base)
        self.vals['Grid Frequency'] = unsigned16(result, 0xBD - base) / 100
        self.vals['Temperature'] = signed16(result, 0xBE - base)
        self.vals['Run Mode 2'] = unsigned16(result, 0xBF - base)
        self.vals['Feedin Power'] = signed32(result, 0xC0 - base)
        self.vals['Battery Voltage'] = signed16(result, 0xC2 - base) / 10
        self.vals['Battery Current'] = signed16(result, 0xC3 - base) / 10
        self.vals['Battery Power'] = signed16(result, 0xC4 - base)
        self.vals['BMS Connected'] = unsigned16(result, 0xC5 - base)
        self.vals['Battery Temperature'] = signed16(result, 0xC6 - base)
        self.vals['Battery Capacity'] = signed16(result, 0xC7 - base)
        self.vals['BMS Warning'] = unsigned16(result, 0xC8 - base)
        self.vals['BMS Charge Max Current'] = unsigned16(result, 0xC9 - base) / 10
        self.vals['BMS Discharge Max Current'] = unsigned16(result, 0xCA - base) / 10
        self.vals['BMS Energy Throughput'] = unsigned32(result, 0xCC - base)

        fullLimit = 95
        if self.vals['name'] in self.config['Solax-BatteryControl']['Inverter']:
            inverter = self.config['Solax-BatteryControl']['Inverter'][self.vals['name']]
            period = self.getPeriod()
            if period is not None and 'grace' in period and 'grace-capacity' in inverter and inverter['grace-capacity'] > 0:
                fullLimit = inverter['grace-capacity']

            self.vals['Battery Demand'] = 0 if self.vals['Battery Capacity'] >= fullLimit else inverter['max-charge']

        self.vals['Power Budget'] = (self.vals['Battery Power'] + self.vals['Measured Power'] +
            self.vals['Measured Power (X3) Phase 1'] + self.vals['Measured Power (X3) Phase 2'] +
            self.vals['Measured Power (X3) Phase 3'])
        self.powerBudgets.append(self.vals['Power Budget'])
        if len(self.powerBudgets) > self.config['Solax-XHybrid-Modbus']['power_budget_avg_samples']:
            del self.powerBudgets[0]
        self.vals['Power Budget Average'] =  sum(self.powerBudgets) / len(self.powerBudgets)

        self.vals['Usage'] = self.vals['Grid Power'] - self.vals['Measured Power']

        self.completionCallback(self.vals, self)

    def wakeupInverter(self, result):
        result = self.factory.getClient().write_register(0x56, 1)

    def tickleRemoteControl(self, result):
        result = self.factory.getClient().write_register(0x51, 1)

    def chargeBattery(self, power):
        self.requestedBatteryPower = power

        if power == 0:
          print("0 power requested for {}\n".format(self.host))
#          traceback.print_stack()

        if power < 0:
            # Convert to int16
            power += 65536
#            result = self.factory.getClient().write_registers(0x07C, [1, power, 0xFFFF, 0, 0])
#        else:
#            result = self.factory.getClient().write_registers(0x07C, [1, power, 0, 0, 0])
        result = self.factory.getClient().write_register(0x52, power)
        inverter = self.config['Solax-BatteryControl']['Inverter'][self.vals['name']]
        if 'tickle-remote-control' in inverter and inverter['tickle-remote-control']:
            result.addCallback(self.tickleRemoteControl)
#        if power != 0:
#            result.addCallback(self.wakeupInverter)


