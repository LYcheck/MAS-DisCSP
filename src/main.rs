use std::collections::HashMap;
use tokio::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

type AgentId = u32;
type Color = u32;

#[derive(Clone, Debug)]
enum Message {
    Ok(AgentId, Color),
    NoGood(AgentId, Vec<(AgentId, Color)>),
    Stop,
}

struct Agent {
    id: AgentId,
    neighbors: Vec<AgentId>,
    colors: Vec<Color>,
    current_value: Option<Color>,
    agent_view: HashMap<AgentId, Color>,
    nogoods: Vec<Vec<(AgentId, Color)>>,
    senders: Arc<HashMap<AgentId, Sender<Message>>>,
    solution_found: Arc<Mutex<HashMap<AgentId, Color>>>,
}

impl Agent {
    fn new(
        id: AgentId,
        neighbors: Vec<AgentId>,
        colors: Vec<Color>,
        senders: Arc<HashMap<AgentId, Sender<Message>>>,
                    solution_found: Arc<Mutex<HashMap<AgentId, Color>>>,
    ) -> Self {
        Agent {
            id,
            neighbors,
            colors,
            current_value: None,
            agent_view: HashMap::new(),
            nogoods: Vec::new(),
            senders,
            solution_found,
        }
    }

    fn is_consistent(&self, value: Color) -> bool {
        // check against agent view
        if !self.agent_view.iter().all(|(&id, &neighbor_color)| {
            !self.neighbors.contains(&id) || value != neighbor_color
        }) {
            return false;
        }

        // check against nogood store
        !self.nogoods.iter().any(|nogood| {
            nogood.iter().all(|(id, color)| {
                if *id == self.id {
                    *color == value
                } else {
                    self.agent_view.get(id) == Some(color)
                }
            })
        })
    }

    fn select_value(&self) -> Option<Color> {
        self.colors.iter()
            .copied()
            .find(|&color| self.is_consistent(color))
    }

    async fn check_solution(&self) {
        // run post consistency check
        if let Some(current_value) = self.current_value {
            if self.neighbors.iter().all(|n| self.agent_view.contains_key(n)) {
                let mut solution = self.solution_found.lock().unwrap();
                solution.insert(self.id, current_value);
                
                // add known neighbour assignments
                for (&id, &color) in &self.agent_view {
                    solution.insert(id, color);
                }
            }
        }
    }

    async fn broadcast_ok(&self) {
        if let Some(value) = self.current_value {
            for &neighbor in &self.neighbors {
                if let Some(sender) = self.senders.get(&neighbor) {
                    let _ = sender.send(Message::Ok(self.id, value)).await;
                }
            }
        }
    }

    async fn run(&mut self, mut receiver: Receiver<Message>) {
        // init default low colour
        if let Some(value) = self.select_value() {
            println!("Agent {} choosing initial value {}", self.id, value);
            self.current_value = Some(value);
            self.broadcast_ok().await;
        }

        while let Some(message) = receiver.recv().await {
            match message {
                Message::Ok(from, value) => {
                    println!("Agent {} received Ok({}, {})", self.id, from, value);
                    self.agent_view.insert(from, value);
                    
                    if let Some(current) = self.current_value {
                        if !self.is_consistent(current) {
                            // try to find new consistent value
                            if let Some(new_value) = self.select_value() {
                                println!("Agent {} changing value to {}", self.id, new_value);
                                self.current_value = Some(new_value);
                                self.broadcast_ok().await;
                            } else {
                                // create nogood
                                let nogood: Vec<(AgentId, Color)> = self.agent_view
                                    .iter()
                                    .filter(|(&id, _)| id < self.id)
                                    .map(|(&id, &color)| (id, color))
                                    .collect();
                                
                                if let Some(&min_id) = nogood.iter().map(|(id, _)| id).min() {
                                    println!("Agent {} sending nogood to {}", self.id, min_id);
                                    if let Some(sender) = self.senders.get(&min_id) {
                                        let _ = sender.send(Message::NoGood(self.id, nogood.clone())).await;
                                    }
                                    // remove assignments from agents ≥ min_id
                                    self.agent_view.retain(|&id, _| id < min_id);
                                    self.current_value = None;
                                }
                            }
                        }
                    }
                    self.check_solution().await;
                }
                Message::NoGood(_, nogood) => {
                    println!("Agent {} received nogood", self.id);
                    // update nogood store
                    self.nogoods.push(nogood);
                    
                    // try to find new consistent value
                    if let Some(new_value) = self.select_value() {
                        println!("Agent {} changing value to {}", self.id, new_value);
                        self.current_value = Some(new_value);
                        self.broadcast_ok().await;
                    }
                    self.check_solution().await;
                }
                Message::Stop => break,
            }
        }
    }
}

pub async fn run_abt(nodes: Vec<(AgentId, Vec<AgentId>)>, colors: Vec<Color>) -> Option<HashMap<AgentId, Color>> {
    let mut receivers = HashMap::new();
    let mut senders = HashMap::new();
    let mut handles = Vec::new();
    
    // create channels
    for (id, _) in &nodes {
        let (sender, receiver) = mpsc::channel(32);
        senders.insert(*id, sender);
        receivers.insert(*id, receiver);
    }
    let senders = Arc::new(senders);
    
    // shared solution flag
    let solution_found = Arc::new(Mutex::new(HashMap::new()));
    
    // start agents
    for (id, neighbors) in &nodes {
        let receiver = receivers.remove(id).unwrap();
        let agent_senders = Arc::clone(&senders);
        let solution_found = Arc::clone(&solution_found);
        
        let mut agent = Agent::new(
            *id,
            neighbors.clone(),
            colors.clone(),
            agent_senders,
            solution_found,
        );
        
        let handle = tokio::spawn(async move {
            agent.run(receiver).await;
        });
        
        handles.push(handle);
    }

    // check periodically for solution
    let mut solution = None;
    let mut consecutive_stable_checks = 0;
    const STABILITY_THRESHOLD: u32 = 1;
    
    while solution.is_none() {
        let assignments = solution_found.lock().unwrap();
        if assignments.len() == nodes.len() {
            // check if all assignments are stable
            let all_consistent = assignments.iter().all(|(&id, &color)| {
                let neighbors: Vec<_> = nodes.iter()
                    .find(|(nid, _)| *nid == id)
                    .map(|(_, neighbors)| neighbors.clone())
                    .unwrap();
                
                neighbors.iter().all(|&neighbor_id| {
                    assignments.get(&neighbor_id)
                        .map(|&neighbor_color| neighbor_color != color)
                        .unwrap_or(true)
                })
            });
            
            if all_consistent {
                consecutive_stable_checks += 1;
                if consecutive_stable_checks >= STABILITY_THRESHOLD {
                    solution = Some(assignments.clone());
                }
            } else {
                consecutive_stable_checks = 0;
            }
        }
        drop(assignments);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // stop agents and clean up
    for sender in senders.values() {
        let _ = sender.send(Message::Stop).await;
    }

    for handle in handles {
        let _ = handle.await;
    }

    solution
}

#[tokio::main]
async fn main() {
    println!("Starting graph coloring with ABT");
    println!("--------------------------------");

    let nodes = vec![
        (1, vec![2, 4]),
        (2, vec![1, 3]),
        (3, vec![2, 4]),
        (4, vec![1, 3]),
    ];

    let colors = vec![1, 2, 3];

    println!("Nodes and their neighbors:");
    for (id, neighbors) in &nodes {
        println!("Node {}: {:?}", id, neighbors);
    }
    println!();

    if let Some(solution) = run_abt(nodes, colors).await {
        println!("Solution found!");
        println!("Final coloring:");
        let mut assignments: Vec<_> = solution.iter().collect();
        assignments.sort_by_key(|(&id, _)| id);
        for (&id, &color) in assignments {
            println!("Node {}: Color {}", id, color);
        }
    } else {
        println!("No solution found.");
    }
}