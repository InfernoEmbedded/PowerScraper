from pymodbus.client.sync import ModbusSerialClient
from pymodbus.exceptions import ModbusException
from twisted.internet import defer, reactor, protocol
from twisted.web.client import Agent, readBody
from pymodbus.constants import Defaults
from twisted.web.http_headers import Headers
from struct import unpack

# import pprint
# pp = pprint.PrettyPrinter(indent=4)

Defaults.UnitId = 1

def float32(result, base, addr):
    low = result.getRegister(addr - base)
    high = result.getRegister(addr - base + 1)
    data = bytearray(4)
    data[0] = high & 0xff
    data[1] = high >> 8
    data[2] = low & 0xff
    data[3] = low >> 8

    val = unpack('f', bytes(data))

    return val[0]

class SDM630ModbusV2(object):

    ##
    # Create a new class to fetch data from the Modbus interface of Solax inverters
    def __init__(self, port, baudrate, parity, stopbits, timeout):
        self.port = port
        self.client = ModbusSerialClient(method="rtu", port=self.port, baudrate=baudrate, parity=parity,
                                         stopbits=stopbits, timeout=timeout)

    def fetch(self, completionCallback):
        base = 0x0000
        result = self.client.read_input_registers(base, 60)
        if isinstance(result, ModbusException):
            print("Exception from SDM630V2: {}".format(result))
            return

        self.vals = {}
        self.vals['name'] = self.port.replace("/dev/tty", "");
        self.vals['Phase 1 line to neutral volts'] = float32(result, base, 0x0000)
        self.vals['Phase 2 line to neutral volts'] = float32(result, base, 0x0002)
        self.vals['Phase 3 line to neutral volts'] = float32(result, base, 0x0004)
        self.vals['Phase 1 current'] = float32(result, base, 0x0006)
        self.vals['Phase 2 current'] = float32(result, base, 0x0008)
        self.vals['Phase 3 current'] = float32(result, base, 0x000A)
        self.vals['Phase 1 power'] = float32(result, base, 0x000C)
        self.vals['Phase 2 power'] = float32(result, base, 0x000E)
        self.vals['Phase 3 power'] = float32(result, base, 0x0010)
        self.vals['Phase 1 volt amps'] = float32(result, base, 0x0012)
        self.vals['Phase 2 volt amps'] = float32(result, base, 0x0014)
        self.vals['Phase 3 volt amps'] = float32(result, base, 0x0016)
        self.vals['Phase 1 volt amps reactive'] = float32(result, base, 0x0018)
        self.vals['Phase 2 volt amps reactive'] = float32(result, base, 0x001A)
        self.vals['Phase 3 volt amps reactive'] = float32(result, base, 0x001C)
        self.vals['Phase 1 power factor'] = float32(result, base, 0x001E)
        self.vals['Phase 2 power factor'] = float32(result, base, 0x0020)
        self.vals['Phase 3 power factor'] = float32(result, base, 0x0022)
        self.vals['Phase 1 phase angle'] = float32(result, base, 0x0024)
        self.vals['Phase 2 phase angle'] = float32(result, base, 0x0026)
        self.vals['Phase 3 phase angle'] = float32(result, base, 0x0028)
        self.vals['Average line to neutral volts'] = float32(result, base, 0x002A)
        self.vals['Average line current'] = float32(result, base, 0x002E)
        self.vals['Sum of line currents'] = float32(result, base, 0x0030)
        self.vals['Total system power'] = float32(result, base, 0x0034)
        self.vals['Total system volt amps'] = float32(result, base, 0x0038)

        base = 0x003C
        result = self.client.read_input_registers(base, 48)
        if isinstance(result, ModbusException):
            print("Exception from SDM630V2: {}".format(result))
            return

        self.vals['Total system VAr'] = float32(result, base, 0x003C)
        self.vals['Total system power factor'] = float32(result, base, 0x003E)
        self.vals['Total system phase angle'] = float32(result, base, 0x0042)
        self.vals['Frequency of supply voltages'] = float32(result, base, 0x0046)
        self.vals['Total import kWh'] = float32(result, base, 0x0048)
        self.vals['Total export kWh'] = float32(result, base, 0x004A)
        self.vals['Total import kVArh '] = float32(result, base, 0x004C)
        self.vals['Total export kVArh '] = float32(result, base, 0x004E)
        self.vals['Total VAh'] = float32(result, base, 0x0050)
        self.vals['Ah'] = float32(result, base, 0x0052)
        self.vals['Total system power demand'] = float32(result, base, 0x0054)
        self.vals['Maximum total system power demand'] = float32(result, base, 0x0056)
        self.vals['Total system VA demand'] = float32(result, base, 0x0064)
        self.vals['Maximum total system VA demand'] = float32(result, base, 0x0066)
        self.vals['Neutral current demand'] = float32(result, base, 0x0068)
        self.vals['Maximum neutral current demand'] = float32(result, base, 0x006A)

        base = 0x00C8
        result = self.client.read_input_registers(base, 8)
        if isinstance(result, ModbusException):
            print("Exception from SDM630V2: {}".format(result))
            return

        self.vals['Line 1 to Line 2 volts'] = float32(result, base, 0x00C8)
        self.vals['Line 2 to Line 3 volts'] = float32(result, base, 0x00CA)
        self.vals['Line 3 to Line 1 volts'] = float32(result, base, 0x00CC)
        self.vals['Average line to line volts'] = float32(result, base, 0x00CE)

        base = 0x00E0
        result = self.client.read_input_registers(base, 46)
        if isinstance(result, ModbusException):
            print("Exception from SDM630V2: {}".format(result))
            return

        self.vals['Neutral current'] = float32(result, base, 0x00E0)
        self.vals['Phase 1 L-N volts THD'] = float32(result, base, 0x00EA)
        self.vals['Phase 2 L-N volts THD'] = float32(result, base, 0x00EC)
        self.vals['Phase 3 L-N volts THD'] = float32(result, base, 0x00EE)
        self.vals['Phase 1 current THD'] = float32(result, base, 0x00F0)
        self.vals['Phase 2 current THD'] = float32(result, base, 0x00F2)
        self.vals['Phase 3 current THD'] = float32(result, base, 0x00F4)
        self.vals['Average line to neutral volts THD'] = float32(result, base, 0x00F8)
        self.vals['Average line current THD'] = float32(result, base, 0x00FA)
        self.vals['Phase 1 current demand'] = float32(result, base, 0x0102)
        self.vals['Phase 2 current demand'] = float32(result, base, 0x0104)
        self.vals['Phase 3 current demand'] = float32(result, base, 0x0106)
        self.vals['Maximum phase 1 current demand'] = float32(result, base, 0x0108)
        self.vals['Maximum phase 2 current demand'] = float32(result, base, 0x010A)
        self.vals['Maximum phase 3 current demand'] = float32(result, base, 0x010C)

        base = 0x014E
        result = self.client.read_input_registers(base, 48)
        if isinstance(result, ModbusException):
            print("Exception from SDM630V2: {}".format(result))
            return

        self.vals['Line 1 to line 2 volts THD'] = float32(result, base, 0x014E)
        self.vals['Line 2 to line 3 volts THD'] = float32(result, base, 0x0150)
        self.vals['Line 3 to line 1 volts THD'] = float32(result, base, 0x0152)
        self.vals['Average line to line volts THD'] = float32(result, base, 0x0154)
        self.vals['Total kWh'] = float32(result, base, 0x0156)
        self.vals['Total kvarh'] = float32(result, base, 0x0158)
        self.vals['Phase 1 import kWh'] = float32(result, base, 0x015a)
        self.vals['Phase 2 import kWh'] = float32(result, base, 0x015c)
        self.vals['Phase 3 import kWh'] = float32(result, base, 0x015e)
        self.vals['Phase 1 export kWh'] = float32(result, base, 0x0160)
        self.vals['Phase 2 export kWh'] = float32(result, base, 0x0162)
        self.vals['Phase 3 export kWh'] = float32(result, base, 0x0164)
        self.vals['Phase 1 total kWh'] = float32(result, base, 0x0166)
        self.vals['Phase 2 total kWh'] = float32(result, base, 0x0168)
        self.vals['Phase 3 total kWh'] = float32(result, base, 0x016a)
        self.vals['Phase 1 import kvarh'] = float32(result, base, 0x016c)
        self.vals['Phase 2 import kvarh'] = float32(result, base, 0x016e)
        self.vals['Phase 3 import kvarh'] = float32(result, base, 0x0170)
        self.vals['Phase 1 export kvarh'] = float32(result, base, 0x0172)
        self.vals['Phase 2 export kvarh'] = float32(result, base, 0x0174)
        self.vals['Phase 3 export kvarh'] = float32(result, base, 0x0176)
        self.vals['Phase 1 total kvarh'] = float32(result, base, 0x0178)
        self.vals['Phase 2 total kvarh'] = float32(result, base, 0x017a)
        self.vals['Phase 3 total kvarh'] = float32(result, base, 0x017c)

        completionCallback(self.vals, None)
