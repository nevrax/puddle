use grid::gridview::{GridSubView, GridView};
use std::fmt;
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;

use plan::PlanError;

#[cfg(feature = "pi")]
use pi::RaspberryPi;

use grid::{
    droplet::{Blob, SimpleBlob},
    Droplet, DropletId, DropletInfo, Grid, Location, Peripheral, Snapshot,
};

use process::{ProcessId, PuddleResult};

pub trait Command: fmt::Debug + Send {
    fn input_droplets(&self) -> Vec<DropletId> {
        vec![]
    }
    fn output_droplets(&self) -> Vec<DropletId> {
        vec![]
    }
    fn bypass(&self, _gridview: &GridView) -> bool {
        false
    }
    // FIXME this shouldn't be mut, but we need to set the collision groups in mix
    fn request(&self, &mut GridView) -> CommandRequest;

    // FIXME this is definitely a hack for combining droplets
    // run before the final routing tick that
    // this better not tick!!!
    fn pre_run(&self, &mut GridSubView) {}

    fn run(&mut self, &mut GridSubView);

    #[cfg(not(feature = "pi"))]
    fn finalize(&mut self, &Snapshot) {}
    #[cfg(feature = "pi")]
    fn finalize(&mut self, &Snapshot, Option<&mut RaspberryPi>) {}

    fn abort(&mut self, err: PlanError) {
        error!("Aborting command {:?} with {:#?}", self, err);
    }
}

pub type BoxedCommand = Box<dyn Command>;

//
//  Create
//

#[derive(Debug)]
pub struct Create {
    inputs: Vec<DropletId>,
    outputs: Vec<DropletId>,
    location: Location,
    dimensions: Location,
    volume: f64,
    trusted: bool,
}

#[derive(Debug)]
pub struct CommandRequest {
    pub shape: Grid,
    pub input_locations: Vec<Location>,
    pub trusted: bool,
}

// TODO: dimensions probably shouldn't be optional?
impl Create {
    pub fn new(
        loc: Option<Location>,
        vol: f64,
        dim: Option<Location>,
        out_id: DropletId,
    ) -> PuddleResult<Create> {
        Ok(Create {
            inputs: vec![],
            outputs: vec![out_id],
            location: loc.unwrap_or(Location { y: 0, x: 0 }),
            dimensions: dim.unwrap_or(Location { y: 1, x: 1 }),
            volume: vol,
            trusted: loc.is_some(),
        })
    }
}

impl Command for Create {
    fn input_droplets(&self) -> Vec<DropletId> {
        self.inputs.clone()
    }

    fn output_droplets(&self) -> Vec<DropletId> {
        self.outputs.clone()
    }

    fn request(&self, _gridview: &mut GridView) -> CommandRequest {
        let grid = Grid::rectangle(self.dimensions.y as usize, self.dimensions.x as usize);

        CommandRequest {
            shape: grid,
            input_locations: vec![],
            trusted: self.trusted,
        }
    }

    fn run(&mut self, gridview: &mut GridSubView) {
        gridview.insert(Droplet::new(
            self.outputs[0],
            self.volume,
            self.location,
            self.dimensions,
        ));
        gridview.tick();
    }
}

//
// Flush
//

pub type FlushResult = Result<Vec<DropletInfo>, PlanError>;

#[derive(Debug)]
pub struct Flush {
    pid: ProcessId,
    tx: Sender<FlushResult>,
}

impl Flush {
    pub fn new(pid: ProcessId, tx: Sender<FlushResult>) -> Flush {
        Flush { pid, tx }
    }
}

impl Command for Flush {
    fn request(&self, _gridview: &mut GridView) -> CommandRequest {
        CommandRequest {
            shape: Grid::rectangle(0, 0),
            input_locations: vec![],
            trusted: false,
        }
    }

    fn run(&mut self, gridview: &mut GridSubView) {
        gridview.tick();
    }

    #[cfg(not(feature = "pi"))]
    fn finalize(&mut self, gv: &Snapshot) {
        let info = gv.droplet_info(Some(self.pid));
        self.tx.send(Ok(info)).unwrap();
    }
    #[cfg(feature = "pi")]
    fn finalize(&mut self, gv: &Snapshot, _: Option<&mut RaspberryPi>) {
        let info = gv.droplet_info(Some(self.pid));
        self.tx.send(Ok(info)).unwrap();
    }

    fn abort(&mut self, err: PlanError) {
        error!("Aborting command {:?} with {:#?}", self, err);
        self.tx.send(Err(err)).unwrap();
    }
}

//
//  Move
//

#[derive(Debug)]
pub struct Move {
    inputs: Vec<DropletId>,
    outputs: Vec<DropletId>,
    destination: [Location; 1],
}

impl Move {
    pub fn new(in_id: DropletId, loc: Location, out_id: DropletId) -> PuddleResult<Move> {
        Ok(Move {
            inputs: vec![in_id],
            outputs: vec![out_id],
            destination: [loc],
        })
    }
}

impl Command for Move {
    fn input_droplets(&self) -> Vec<DropletId> {
        self.inputs.clone()
    }

    fn output_droplets(&self) -> Vec<DropletId> {
        self.outputs.clone()
    }

    fn request(&self, gridview: &mut GridView) -> CommandRequest {
        let old_id = self.inputs[0];
        let dim = gridview.snapshot().droplets[&old_id].dimensions;
        CommandRequest {
            shape: Grid::rectangle(dim.y as usize, dim.x as usize),
            input_locations: vec![self.destination[0]],
            trusted: true,
        }
    }

    fn run(&mut self, gridview: &mut GridSubView) {
        let old_id = self.inputs[0];
        let new_id = self.outputs[0];
        let mut d = gridview.remove(&old_id);
        // NOTE this is pretty much the only place it's ok to change an id
        d.id = new_id;
        gridview.insert(d);
        gridview.tick()
    }
}

//
//  Combine
//

#[derive(Debug)]
pub struct Combine {
    inputs: Vec<DropletId>,
    outputs: Vec<DropletId>,
    pin_d0: bool,
}

impl Combine {
    pub fn new(id1: DropletId, id2: DropletId, out_id: DropletId) -> PuddleResult<Combine> {
        Ok(Combine {
            inputs: vec![id1, id2],
            outputs: vec![out_id],
            pin_d0: false,
        })
    }

    // combines the second into the first, pinning the first
    pub fn combine_into(
        id1: DropletId,
        id2: DropletId,
        out_id: DropletId,
    ) -> PuddleResult<Combine> {
        Ok(Combine {
            inputs: vec![id1, id2],
            outputs: vec![out_id],
            pin_d0: true,
        })
    }

    fn combined(&self, d0: &Droplet, d1: &Droplet) -> SimpleBlob {
        // FIXME this is a hack
        // right now we only support vertical stacking
        if self.pin_d0 {
            assert!(d0.location.y > d1.dimensions.y);
        }
        SimpleBlob {
            location: &d0.location - &Location {
                y: d1.dimensions.y,
                x: 0,
            },
            dimensions: Location {
                y: (d0.dimensions.y + d1.dimensions.y),
                x: d0.dimensions.x.max(d1.dimensions.x),
            },
            volume: d0.volume + d1.volume,
        }
    }
}

impl Command for Combine {
    fn input_droplets(&self) -> Vec<DropletId> {
        self.inputs.clone()
    }

    fn output_droplets(&self) -> Vec<DropletId> {
        self.outputs.clone()
    }

    fn bypass(&self, gridview: &GridView) -> bool {
        let droplets = &gridview.snapshot().droplets;
        if droplets.contains_key(&self.outputs[0]) {
            assert!(!droplets.contains_key(&self.inputs[0]));
            assert!(!droplets.contains_key(&self.inputs[1]));
            true
        } else {
            false
        }
    }

    fn request(&self, gridview: &mut GridView) -> CommandRequest {
        let droplets = &mut gridview.snapshot_mut().droplets;

        let id0 = &self.inputs[0];
        let id1 = &self.inputs[1];

        // set the collision groups to be the same
        // must scope the mutable borrow
        {
            let cg1 = droplets[id1].collision_group;
            let d0 = droplets.get_mut(id0).unwrap();
            d0.collision_group = cg1;

            if self.pin_d0 {
                d0.pinned = true;
            }
        }

        let d0 = &droplets[id0];
        let d1 = &droplets[id1];

        let combined = self.combined(d0, d1);

        if self.pin_d0 {
            CommandRequest {
                shape: Grid::rectangle(
                    combined.dimensions.y as usize,
                    combined.dimensions.x as usize,
                ),
                input_locations: vec![d0.location, combined.location],
                trusted: true,
            }
        } else {
            CommandRequest {
                shape: Grid::rectangle(
                    combined.dimensions.y as usize,
                    combined.dimensions.x as usize,
                ),
                input_locations: vec![
                    Location {
                        y: d1.dimensions.y,
                        x: 0,
                    },
                    Location { y: 0, x: 0 },
                ],
                trusted: false,
            }
        }
    }

    fn pre_run(&self, gridview: &mut GridSubView) {
        let in0 = self.inputs[0];
        let in1 = self.inputs[1];
        let out = self.outputs[0];

        let d0 = gridview.remove(&in0);
        let d1 = gridview.remove(&in1);
        // TODO right now this only mixes vertical
        // it should somehow communicate with the Combine command to control the mixed droplets dimensions
        let combined = self.combined(&d0, &d1);

        // assert_eq!(d0.location.y, d1.location.y);
        // assert_eq!(d0.location.x + d0.dimensions.x, d1.location.x);
        gridview.insert(combined.to_droplet(out));
    }

    fn run(&mut self, _: &mut GridSubView) {}
}

//
//  Agitate
//

#[derive(Debug)]
pub struct Agitate {
    inputs: Vec<DropletId>,
    outputs: Vec<DropletId>,
    n_agitation_loops: u32,
}

impl Agitate {
    pub fn new(in_id: DropletId, out_id: DropletId) -> PuddleResult<Agitate> {
        Ok(Agitate {
            inputs: vec![in_id],
            outputs: vec![out_id],
            n_agitation_loops: 1,
        })
    }
}

const AGITATE_PADDING: usize = 1;

impl Command for Agitate {
    fn input_droplets(&self) -> Vec<DropletId> {
        self.inputs.clone()
    }

    fn output_droplets(&self) -> Vec<DropletId> {
        self.outputs.clone()
    }

    fn request(&self, gridview: &mut GridView) -> CommandRequest {
        let droplets = &mut gridview.snapshot_mut().droplets;
        let droplet = droplets.get_mut(&self.inputs[0]).unwrap();

        CommandRequest {
            shape: Grid::rectangle(
                droplet.dimensions.y as usize + AGITATE_PADDING,
                droplet.dimensions.x as usize + AGITATE_PADDING,
            ),
            input_locations: vec![Location { y: 0, x: 0 }],
            trusted: false,
        }
    }

    fn run(&mut self, gridview: &mut GridSubView) {
        let in_id = self.inputs[0];
        let out_id = self.outputs[0];

        for i in 0..self.n_agitation_loops {
            debug!("Agitating droplet {:?}, iteration {}", in_id, i);
            gridview.move_south(in_id);
            gridview.tick();
            gridview.move_east(in_id);
            gridview.tick();
            gridview.move_north(in_id);
            gridview.tick();
            gridview.move_west(in_id);
            gridview.tick();
        }

        let mut droplet = gridview.remove(&in_id);
        droplet.id = out_id;
        gridview.insert(droplet)
    }
}

//
//  Split
//

#[derive(Debug)]
pub struct Split {
    inputs: Vec<DropletId>,
    outputs: Vec<DropletId>,
}

impl Split {
    pub fn new(id: DropletId, out_id1: DropletId, out_id2: DropletId) -> PuddleResult<Split> {
        Ok(Split {
            inputs: vec![id],
            outputs: vec![out_id1, out_id2],
        })
    }
}

const SPLIT_PADDING: usize = 4;

impl Command for Split {
    fn input_droplets(&self) -> Vec<DropletId> {
        self.inputs.clone()
    }

    fn output_droplets(&self) -> Vec<DropletId> {
        self.outputs.clone()
    }

    fn bypass(&self, gridview: &GridView) -> bool {
        let droplets = &gridview.snapshot().droplets;
        // if it has one, it better have both
        if droplets.contains_key(&self.outputs[0]) || droplets.contains_key(&self.outputs[1]) {
            assert!(droplets.contains_key(&self.outputs[0]));
            assert!(droplets.contains_key(&self.outputs[1]));
            assert!(!droplets.contains_key(&self.inputs[0]));
            true
        } else {
            false
        }
    }

    fn request(&self, gridview: &mut GridView) -> CommandRequest {
        let droplets = &gridview.snapshot().droplets;
        let d0 = droplets.get(&self.inputs[0]).unwrap();
        // we only split in the x right now, so we don't need y padding
        let x_dim = (d0.dimensions.x as usize) + SPLIT_PADDING;
        let y_dim = d0.dimensions.y as usize;
        let grid = Grid::rectangle(y_dim, x_dim);

        let input_locations = vec![Location { y: 0, x: 2 }];

        CommandRequest {
            shape: grid,
            input_locations: input_locations,
            trusted: false,
        }
    }

    fn run(&mut self, gridview: &mut GridSubView) {
        let x_dim = {
            // limit the scope of d0 borrow
            let d0 = gridview.get(&self.inputs[0]);
            (d0.dimensions.x as usize) + SPLIT_PADDING
        };

        let inp = self.inputs[0];
        let out0 = self.outputs[0];
        let out1 = self.outputs[1];

        let d = gridview.remove(&inp);
        let vol = d.volume / 2.0;

        // TODO: this should be related to volume in some fashion
        // currently, take the ceiling of the division of the split by two
        let dim = Location {
            y: d.dimensions.y,
            x: (d.dimensions.x + 1) / 2,
        };

        let loc0 = Location { y: 0, x: 1 };
        let loc1 = Location {
            y: 0,
            x: x_dim as i32 - (dim.x + 1),
        };

        gridview.insert(Droplet::new(out0, vol, loc0, dim));
        gridview.insert(Droplet::new(out1, vol, loc1, dim));

        gridview.tick();
        gridview.move_west(out0);
        gridview.move_east(out1);
        gridview.tick();
    }
}

#[derive(Debug)]
pub struct Heat {
    inputs: Vec<DropletId>,
    outputs: Vec<DropletId>,
    temperature: f32,
    duration: Duration,
    heater: Option<Peripheral>,
}

impl Heat {
    pub fn new(
        id: DropletId,
        out_id: DropletId,
        temperature: f32,
        duration: Duration,
    ) -> PuddleResult<Heat> {
        Ok(Heat {
            inputs: vec![id],
            outputs: vec![out_id],
            temperature,
            duration,
            heater: None,
        })
    }
}

impl Command for Heat {
    fn input_droplets(&self) -> Vec<DropletId> {
        self.inputs.clone()
    }

    fn output_droplets(&self) -> Vec<DropletId> {
        self.outputs.clone()
    }

    fn request(&self, gridview: &mut GridView) -> CommandRequest {
        let droplets = &gridview.snapshot().droplets;
        let d = droplets.get(&self.inputs[0]).unwrap();
        // we only split in the x right now, so we don't need y padding
        let x_dim = d.dimensions.x as usize;
        let y_dim = d.dimensions.y as usize;

        // right now we can only heat droplets that are 1x1
        // assert_eq!(y_dim, 1);
        assert_eq!(x_dim, 1);
        let mut grid = Grid::rectangle(y_dim, x_dim);

        // the parameters of heater here don't matter, as it's just used to
        // match up with the "real" heater in the actual grid
        let loc = Location {
            y: y_dim as i32 - 1,
            x: 0,
        };
        grid.get_cell_mut(&loc).unwrap().peripheral = Some(Peripheral::Heater {
            pwm_channel: 0,
            spi_channel: 0,
        });

        let input_locations = vec![loc];

        CommandRequest {
            shape: grid,
            input_locations: input_locations,
            trusted: false,
        }
    }

    fn run(&mut self, gridview: &mut GridSubView) {
        #[cfg(feature = "pi")]
        {
            let d = gridview.get(&self.inputs[0]);
            let loc = Location {
                y: d.dimensions.y - 1,
                x: 0,
            };
            let heater = gridview
                .get_electrode(&loc)
                .cloned()
                .unwrap()
                .peripheral
                .unwrap();
            assert_matches!(heater, Peripheral::Heater{..});
            self.heater = Some(heater)
        }
        let old_id = self.inputs[0];
        let new_id = self.outputs[0];

        let mut d = gridview.remove(&old_id);
        // NOTE this is a rare place it's ok to change an id, like move
        d.id = new_id;
        gridview.insert(d);
        gridview.tick()
    }

    #[cfg(feature = "pi")]
    fn finalize(&mut self, _: &Snapshot, pi: Option<&mut RaspberryPi>) {
        let heater = self.heater.take().unwrap();
        pi.map(|pi| pi.heat(&heater, self.temperature as f64, self.duration));
    }
}

#[derive(Debug)]
pub struct Input {
    substance: String,
    volume: f64,
    dimensions: Location,
    outputs: Vec<DropletId>,
    input: Option<Peripheral>,
}

impl Input {
    pub fn new(
        substance: String,
        volume: f64,
        dimensions: Location,
        out_id: DropletId,
    ) -> PuddleResult<Input> {
        Ok(Input {
            substance,
            volume,
            dimensions,
            outputs: vec![out_id],
            input: None,
        })
    }
}

impl Command for Input {
    fn input_droplets(&self) -> Vec<DropletId> {
        vec![]
    }

    fn output_droplets(&self) -> Vec<DropletId> {
        self.outputs.clone()
    }

    fn request(&self, _gridview: &mut GridView) -> CommandRequest {
        let mut grid = Grid::rectangle(self.dimensions.y as usize, self.dimensions.x as usize + 1);

        // fake peripheral used to match up with the real one
        // FIXME: this is a total hack to assume that input is always on the right-hand side
        let loc = Location {
            y: self.dimensions.y / 2,
            x: self.dimensions.x - 1 + 1,
        };
        grid.get_cell_mut(&loc).unwrap().peripheral = Some(Peripheral::Input {
            pwm_channel: 0,
            name: self.substance.clone(),
        });

        debug!("Input location will be at {}", loc);

        CommandRequest {
            shape: grid,
            input_locations: vec![],
            trusted: false,
        }
    }

    fn run(&mut self, gridview: &mut GridSubView) {
        // FIXME: this is a total hack to assume that input is always on the right-hand side
        let input_loc = Location {
            y: self.dimensions.y / 2,
            x: self.dimensions.x - 1 + 1,
        };
        #[cfg(feature = "pi")]
        {
            let input = gridview
                .get_electrode(&input_loc)
                .cloned()
                .unwrap()
                .peripheral
                .unwrap();
            assert_matches!(input, Peripheral::Input{..});
            self.input = Some(input);
        }
        let new_id = self.outputs[0];

        let d_loc = Location { y: 0, x: 0 };
        let d = Droplet::new(new_id, self.volume, d_loc, self.dimensions);
        gridview.insert(d);
        gridview.tick()
    }

    #[cfg(feature = "pi")]
    fn finalize(&mut self, _: &Snapshot, pi: Option<&mut RaspberryPi>) {
        let input = self.input.take().unwrap();
        pi.map(|pi| {
            let loc26 = 118;
            let loc27 = 119;
            let loc36 = 112;
            let loc37 = 113;

            let set = |pi: &mut RaspberryPi, pins: &[usize]| {
                let mut all_pins = [0; 128];
                for p in pins {
                    all_pins[*p] = 1;
                }
                pi.bad_manual_output_pins(&all_pins);
            };

            set(pi, &[loc26, loc27, loc36, loc37]);
            pi.input(&input, self.volume).unwrap();

            set(pi, &[loc27]);
            thread::sleep(Duration::from_millis(1500));

            set(pi, &[loc37]);
            thread::sleep(Duration::from_millis(1500));

            set(pi, &[loc26, loc27, loc36, loc37]);
            thread::sleep(Duration::from_millis(1500));

            set(pi, &[loc26]);
            thread::sleep(Duration::from_millis(1500));
        });
    }
}

#[derive(Debug)]
pub struct Output {
    name: String,
    inputs: Vec<DropletId>,
    volume: Option<f64>,
    output: Option<Peripheral>,
}

impl Output {
    pub fn new(name: String, id: DropletId) -> PuddleResult<Output> {
        Ok(Output {
            name,
            inputs: vec![id],
            volume: None,
            output: None,
        })
    }
}

impl Command for Output {
    fn input_droplets(&self) -> Vec<DropletId> {
        self.inputs.clone()
    }

    fn output_droplets(&self) -> Vec<DropletId> {
        vec![]
    }

    fn request(&self, gridview: &mut GridView) -> CommandRequest {
        let droplets = &gridview.snapshot().droplets;
        let d = droplets.get(&self.inputs[0]).unwrap();

        let mut grid = Grid::rectangle(d.dimensions.y as usize, d.dimensions.x as usize);

        // fake peripheral used to match up with the real one
        // FIXME: this is a total hack to assume that output is always on the left-hand side
        let loc = Location {
            y: d.dimensions.y / 2,
            x: 0,
        };
        grid.get_cell_mut(&loc).unwrap().peripheral = Some(Peripheral::Output {
            pwm_channel: 0,
            name: self.name.clone(),
        });

        debug!("Output location will be at {}", loc);

        CommandRequest {
            shape: grid,
            input_locations: vec![loc],
            trusted: false,
        }
    }

    fn run(&mut self, gridview: &mut GridSubView) {
        let id = self.inputs[0];
        #[cfg(feature = "pi")]
        {
            // FIXME: this is a total hack to assume that output is always on the left-hand side
            let loc = Location {
                y: gridview.get(&id).dimensions.y / 2,
                x: 0,
            };
            let volume = gridview.get(&id).volume;
            let output = gridview
                .get_electrode(&loc)
                .cloned()
                .unwrap()
                .peripheral
                .unwrap();
            assert_matches!(output, Peripheral::Output{..});
            self.output = Some(output);
            self.volume = Some(volume);
            // gridview.with_pi(|pi| pi.output(&output, volume));
        }
        gridview.remove(&id);
        gridview.tick()
    }

    #[cfg(feature = "pi")]
    fn finalize(&mut self, _: &Snapshot, pi: Option<&mut RaspberryPi>) {
        let volume = self.volume.take().unwrap();
        let output = self.output.take().unwrap();
        pi.map(|pi| pi.output(&output, volume));
    }
}
