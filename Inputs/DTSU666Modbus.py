from pymodbus.client.sync import ModbusSerialClient
from pymodbus.exceptions import ModbusException
from struct import unpack

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

class DTSU666Modbus(object):
    # Class to interface with the DTSU666 energy meter via Modbus RTU

    def __init__(self, port, baudrate=9600, parity='N', stopbits=1, timeout=3):
        self.port = port
        # Initialize Modbus serial client with default parameters for DTSU666
        self.client = ModbusSerialClient(method="rtu", port=port, baudrate=baudrate, 
                                         parity=parity, stopbits=stopbits, timeout=timeout)

    def fetch(self, completionCallback):
        try:
            # Reading first block of registers
            base = 0x2000
            result = self.client.read_input_registers(base, 0x52, unit=1)
            if isinstance(result, ModbusException):
                print("Exception from DTSU666: {}".format(result))
                return
        
            # Parsing registers from the first block
            self.vals = {}
            self.vals['name'] = self.port.replace("/dev/tty", "");
            self.vals['Line 1 to Line 2 volts'] =  float32(result, base, 0x2000) / 10;
            self.vals['Line 2 to Line 3 volts'] =  float32(result, base, 0x2002) / 10;
            self.vals['Line 3 to Line 1 volts'] =  float32(result, base, 0x2004) / 10;
            self.vals['Phase 1 line to neutral volts'] =  float32(result, base, 0x2006) / 10;
            self.vals['Phase 2 line to neutral volts'] =  float32(result, base, 0x2008) / 10;
            self.vals['Phase 3 line to neutral volts'] =  float32(result, base, 0x200A) / 10;
            self.vals['Phase 1 current'] =  float32(result, base, 0x200C) / 1000;
            self.vals['Phase 2 current'] =  float32(result, base, 0x200E) / 1000;
            self.vals['Phase 3 current'] =  float32(result, base, 0x2010) / 1000;
            self.vals['Phase 1 power'] =  float32(result, base, 0x2014) / 10;
            self.vals['Phase 2 power'] =  float32(result, base, 0x2016) / 10;
            self.vals['Phase 3 power'] =  float32(result, base, 0x2018) / 10;
            self.vals['Phase 1 volt amps reactive'] =  float32(result, base, 0x201C) / 10;
            self.vals['Phase 2 volt amps reactive'] =  float32(result, base, 0x201E) / 10;
            self.vals['Phase 3 volt amps reactive'] =  float32(result, base, 0x2020) / 10;
            self.vals['Phase 1 power factor'] =  float32(result, base, 0x202C) / 1000;
            self.vals['Phase 2 power factor'] =  float32(result, base, 0x202E) / 1000;
            self.vals['Phase 3 power factor'] =  float32(result, base, 0x2030) / 1000;
            self.vals['Total system power'] =  float32(result, base, 0x2012) / 10;
            self.vals['Total system VAr'] =  float32(result, base, 0x201A) / 10;
            self.vals['Total system power factor'] =  float32(result, base, 0x202A) / 1000;
            self.vals['Frequency Of supply voltages'] =  float32(result, base, 0x2044) / 100;
            self.vals['Total system power demand'] =  float32(result, base, 0x2044) / 10;

            # Reading second block of registers
            base = 0x401E
            result = self.client.read_input_registers(base, 52, unit=1)
            if isinstance(result, ModbusException):
                print("Exception from DTSU666: {}".format(result))
                return

            # Parsing registers from the second block
            # Adjusting the base address and register address accordingly
            self.vals['Total import kWh'] =  float32(result, base, 0x401E) * 1000;
            self.vals['Total export kWh'] =  float32(result, base, 0x4028) * 1000;
            self.vals['Total Q1 kvarh'] =  float32(result, base, 0x4032) * 1000;
            self.vals['Total Q2 kvarh'] =  float32(result, base, 0x403C) * 1000;
            self.vals['Total Q3 kvarh'] =  float32(result, base, 0x4046) * 1000;
            self.vals['Total Q4 kvarh'] =  float32(result, base, 0x4050) * 1000;

            completionCallback(self.vals, None)
        
        except ModbusException as e:
            print(f"Modbus Error: {e}")
            return None
