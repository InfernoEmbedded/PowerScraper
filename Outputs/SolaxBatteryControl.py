import datetime
from time import sleep

import pprint
pp = pprint.PrettyPrinter(indent=4)

class SolaxBatteryControl(object):
    def __init__(self, config):
        self.config = config
        self.phasePower = [0]*16
        self.assistNeeded = {}
        self.totalPower = 0
        self.totalDischargePower = 0

    def handleMeterPower(self, vals):
        self.totalPower = vals['Total system power']
        self.phasePower[1] = vals['Phase 1 power']
        self.phasePower[2] = vals['Phase 2 power']
        self.phasePower[3] = vals['Phase 3 power']

    def handleInverterPower(self, phase, vals):
        self.phasePower[phase] = vals['Feed In Power'] * -1
        self.totalPower = self.phasePower[1] + self.phasePower[2] + self.phasePower[3]

    def getPeriod(self):
        nowDateTime = datetime.datetime.now()
        now = datetime.time(nowDateTime.hour, nowDateTime.minute, nowDateTime.second)
        midnight = datetime.time(0, 0, 0)

        for periodName, period in self.config['Period'].items():
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

    def dischargeAt(self, client, power):
        power = int(power) * -1

        # Convert to int16
        if power < 0:
            power += 65536

        result = client.write_register(0x51, power)

    def inverterAwake(self, result, client, power):
        self.dischargeAt(client, power)

    def enableGridService(self, client, power):
        result = client.write_register(0x92, 1)
        result.addCallback(self.inverterAwake, client, power)

    def wakeupInverter(self, client, power):
        result = client.write_register(0x90, 1)
        result.addCallback(self.enableGridService(result, client, power))

    def assistancePower(self):
        for inverter, assistNeeded in self.assistNeeded.items():
            if assistNeeded:
                return True

        return False

    def send(self, vals):
        valCopy = vals.copy()
        inverterName = valCopy.pop('name', None)

        if 'source' in self.config:
            if inverterName == self.config['source']:
                self.handleMeterPower(valCopy)
                return

        if inverterName not in self.config['Inverter']:
            print("Inverter {} not found\n".format(inverterName))
            pp.pprint(self.config)
            return

        period = self.getPeriod()
        if period is None:
            return

        inverter = self.config['Inverter'][inverterName]
        if 'DischargePower' not in inverter:
            inverter['DischargePower'] = 0

        phase = inverter['phase']

        if not 'source' in self.config:
                #print("Using inverter power")
                self.handleInverterPower(inverter, valCopy)


        # If we allow charging from the grid, charge up to the minimum capacity
        if period['grid-charge'] and vals['Battery Capacity'] < period['min-charge']:
            inverter['DischargePower'] = inverter['max-charge'] * -1
            #print("{} charging from the grid at {}W".format(inverterName, inverter['DischargePower'] * -1))
            self.enableGridService(vals['#SolaxClient'], inverter['DischargePower'])
            # Don't expect other inverters to take the load if power is cheap enough to charge
            # otherwise, we just shift power from one inverter to another and suffer conversion losses
            # along the way
            self.assistNeeded[inverterName] = False
            return

        # Try and zero our phase power
        #print("Initial discharge power is {}, additional from phase is {}\n".format(inverter['DischargePower'], self.phasePower[phase]))
        inverter['DischargePower'] += self.phasePower[phase] * 0.25

        self.assistNeeded[inverterName] = False

        # Do we need help servicing the load?
        if (inverter['DischargePower'] + self.phasePower[phase] * 0.75) > inverter['single-phase-discharge-limit']:
            #print("{} Discharge exceeded".format(inverterName))
            self.assistNeeded[inverterName] = True
        elif (inverter['DischargePower'] + self.phasePower[phase] * 0.75) < inverter['single-phase-charge-limit'] * -1:
            #print("{} Charge exceeded".format(inverterName))
            self.assistNeeded[inverterName] = True
        else:
            #print("{} In range for single phase".format(inverterName))
            self.assistNeeded[inverterName] = False

        # Don't discharge below the minimum allowed for this period
        if vals['Battery Capacity'] <= period['min-charge'] and inverter['DischargePower'] > 0:
            inverter['DischargePower'] = 0
            #print("{} not discharging as capacity is low ({} <= {})".format(inverterName, vals['Battery Capacity'], period['min-charge']))
            self.dischargeAt(vals['#SolaxClient'],  inverter['DischargePower'])
            self.assistNeeded[inverterName] = True
            return

        if self.assistancePower():
            #print("{} Assistance needed".format(inverterName))
            # Load share between phases
            #print("Load sharing activated, Total power is {}".format(self.totalPower))
            if self.config['linked-batteries']:
                self.totalDischargePower += self.totalPower * 0.1
                inverter['DischargePower'] = self.totalDischargePower / len(self.config['Inverter'])
                #print("{} totalPower delta = {}  Linked assistance power = {}".format(inverterName, self.totalPower, inverter['DischargePower']))
            else:
                inverter['DischargePower'] -= self.phasePower[phase] * 0.25
                inverter['DischargePower'] += self.totalPower * 0.1
        #else:
           # print("Phase {} power is {}".format(phase, self.phasePower[phase]))

        # Clamp charge & discharge power
        if inverter['DischargePower'] > inverter['max-discharge']:
            inverter['DischargePower'] = inverter['max-discharge']
        elif inverter['DischargePower'] < inverter['max-charge'] * -1:
            inverter['DischargePower'] = inverter['max-charge'] * -1

        #print("{} to discharge at {}W".format(inverterName, inverter['DischargePower']))
        self.dischargeAt(vals['#SolaxClient'],  inverter['DischargePower'])


