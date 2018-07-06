from pymodbus.client.async import ModbusClientProtocol, ModbusClientFactory
from pymodbus.framer.rtu_framer import ModbusRtuFramer
from twisted.internet import defer, reactor, protocol
from twisted.web.client import Agent, readBody
#import unicodedata
from twisted.web.http_headers import Headers

#import pprint
#pp = pprint.PrettyPrinter(indent=4)


def unsigned16(result, addr):
    return result.getRegister(addr)
    
    if val > 32767:
        val -= 32768
    return val

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

    def setClient(self, client):
        self.client = client

    def readRegisters(self):
        if self.client != None:
            return self.client.read_input_registers(0, 0x72)
        else:
            print("not connected")

    def buildProtocol(self, addr):
        self.resetDelay()
        p = SolaxProtocol()
        p.factory = self
        return p    

class SolaxModbus(object):

    ##
    # Create a new class to fetch data from the Modbus interface of Solax inverters
    def __init__(self, host):
        self.host = host        
        self.factory = SolaxFactory()
        reactor.connectTCP(host, 502, self.factory)

#         defer = protocol.ClientCreator(reactor, ModbusClientProtocol).connectTCP(host, 502)
#         defer.addCallback(self.setClient)
#          
#     def setClient(self, client):
#         self.client = client
        
    def fetch(self, completionCallback):
        result = self.factory.readRegisters()
        if result != None:
            result.addCallback(self.solaxRegisterCallback, completionCallback)
        
    def solaxRegisterCallback(self, result, completionCallback):
        vals = {}
        vals['inverter'] = self.host;
        vals['Grid Voltage'] = unsigned16(result, 0x00) / 10
        vals['Grid Current'] = signed16(result, 0x01) / 10
        vals['Grid Power'] = signed16(result, 0x02) / 10
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
        vals['Battery Energy Charged'] = unsigned32(result, 0x1D) / 10 
        vals['BMS Warning'] = unsigned16(result, 0x1F)
        vals['Battery Energy Discharged'] = unsigned32(result, 0x20) / 10
        vals['Battery State of Health'] = unsigned16(result, 0x23)
        vals['Feed In Power'] = signed32(result, 0x46) # Power to the grid
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
        
        completionCallback(vals)
